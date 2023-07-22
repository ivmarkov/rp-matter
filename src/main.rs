#![no_std]
#![no_main]
#![feature(type_alias_impl_trait)]

use core::borrow::Borrow;

use embassy_executor::Spawner;
use embassy_futures::yield_now;
use embassy_net::{Stack, StackResources};
use embassy_net_w5500::*;
use embassy_rp::bind_interrupts;
use embassy_rp::clocks::RoscRng;
use embassy_rp::gpio::{Input, Level, Output, Pull};
use embassy_rp::peripherals::{DMA_CH0, DMA_CH1, PIN_10, PIN_11, PIN_8, USB};
use embassy_rp::peripherals::{PIN_12, PIN_13, PIN_9, SPI1};
use embassy_rp::spi::{Async, Config as SpiConfig, Spi};
use embassy_rp::usb::{Driver, InterruptHandler};
use embassy_time::Delay;
use embassy_time::{Duration, Timer};
use embedded_alloc::Heap;
use embedded_hal_async::spi::ExclusiveDevice;

use log::{error, info};

use rs_matter::core::{CommissioningData, Matter};
use rs_matter::data_model::cluster_basic_information::BasicInfoConfig;
use rs_matter::data_model::cluster_on_off;
use rs_matter::data_model::device_types::DEV_TYPE_ON_OFF_LIGHT;
use rs_matter::data_model::objects::*;
use rs_matter::data_model::root_endpoint;
use rs_matter::data_model::system_model::descriptor;
use rs_matter::error::Error;
use rs_matter::mdns::{MdnsRunBuffers, MdnsService};
use rs_matter::secure_channel::spake2p::VerifierData;
use rs_matter::transport::core::RunBuffers;
use rs_matter::transport::network::{Ipv4Addr, Ipv6Addr};

use rand::RngCore;
use smoltcp::wire::{Ipv6Address, Ipv6Cidr};
use static_cell::make_static;

//use defmt::*;
//use defmt_rtt as _;
//use panic_probe as _;

mod dev_att;

bind_interrupts!(struct Irqs {
    USBCTRL_IRQ => InterruptHandler<USB>;
});

type W5500Runner = Runner<
    'static,
    ExclusiveDevice<Spi<'static, SPI1, Async>, Output<'static, PIN_9>, Delay>,
    Input<'static, PIN_12>,
    Output<'static, PIN_13>,
>;

#[global_allocator]
static HEAP: Heap = Heap::empty();

#[embassy_executor::main]
async fn main(mut spawner: Spawner) {
    let p = embassy_rp::init(Default::default());

    let driver = Driver::new(p.USB, Irqs);
    spawner.spawn(logger_task(driver)).unwrap();

    for secs in (0..6).rev() {
        info!("Starting in {secs} secs...");
        Timer::after(Duration::from_secs(1)).await;
    }

    init_allocator();

    let device = create_ethernet_device(
        p.SPI1,
        p.DMA_CH0,
        p.DMA_CH1,
        p.PIN_8,
        p.PIN_11,
        p.PIN_9,
        p.PIN_10,
        p.PIN_12,
        p.PIN_13,
        &mut spawner,
    )
    .await;

    let stack = create_ip_stack(device, &mut spawner).await;

    let (cfg, cfg_v6) = wait_for_ip_config(stack).await;

    let local_addr = cfg.address.address();
    let local_addr_v6 = cfg_v6.address.address();

    info!("IP address: {:?}, V6: {:?}", local_addr, local_addr_v6);

    // Create & launch Matter now

    run_matter(
        stack,
        Ipv4Addr::from(local_addr.0),
        Some(Ipv6Addr::from(local_addr_v6.0)),
        &mut spawner,
    )
    .await
    .unwrap();
}

async fn run_matter(
    stack: &'static Stack<Device<'static>>,
    ipv4_addr: Ipv4Addr,
    ipv6_addr: Option<Ipv6Addr>,
    spawner: &mut Spawner,
) -> Result<(), Error> {
    info!(
        "Matter memory: mDNS={}, Matter={}, MdnsBuffers={}, RunBuffers={}",
        core::mem::size_of::<MdnsService>(),
        core::mem::size_of::<Matter>(),
        core::mem::size_of::<MdnsRunBuffers>(),
        core::mem::size_of::<RunBuffers>(),
    );

    let dev_det = &*make_static!(BasicInfoConfig {
        vid: 0xFFF1,
        pid: 0x8000,
        hw_ver: 2,
        sw_ver: 1,
        sw_ver_str: "1",
        serial_no: "aabbccdd",
        device_name: "OnOff Light",
    });

    let dev_att = &*make_static!(dev_att::HardCodedDevAtt::new());

    let mdns = &*make_static!(MdnsService::new(
        0,
        "matter-demo",
        ipv4_addr.octets(),
        ipv6_addr.map(|ip| (ip.octets(), 0)),
        dev_det,
        rs_matter::MATTER_PORT,
    ));

    info!("mDNS initialized");

    let matter: &Matter<'static> = &*make_static!(Matter::new(
        // vid/pid should match those in the DAC
        dev_det,
        dev_att,
        mdns,
        epoch,
        rand,
        rs_matter::MATTER_PORT,
    ));

    info!("Matter initialized, starting...");

    spawner
        .spawn(mdns_task(mdns, stack, make_static!(MdnsRunBuffers::new())))
        .unwrap();

    let handler = &*make_static!(HandlerCompat(handler(matter)));

    matter
        .run(
            stack,
            make_static!(RunBuffers::new()),
            CommissioningData {
                // TODO: Hard-coded for now
                verifier: VerifierData::new_with_pw(123456, *matter.borrow()),
                discriminator: 250,
            },
            handler,
        )
        .await
}

#[embassy_executor::task]
async fn mdns_task(
    mdns: &'static MdnsService<'static>,
    stack: &'static Stack<Device<'static>>,
    buffers: &'static mut MdnsRunBuffers,
) {
    mdns.run(stack, buffers).await.unwrap();
}

#[embassy_executor::task]
async fn logger_task(driver: Driver<'static, USB>) {
    embassy_usb_logger::run!(1024, log::LevelFilter::Info, driver);
}

#[embassy_executor::task]
async fn ethernet_task(runner: W5500Runner) -> ! {
    runner.run().await
}

#[embassy_executor::task]
async fn net_task(stack: &'static Stack<Device<'static>>) -> ! {
    stack.run().await
}

const NODE: Node<'static> = Node {
    id: 0,
    endpoints: &[
        root_endpoint::endpoint(0),
        Endpoint {
            id: 1,
            device_type: DEV_TYPE_ON_OFF_LIGHT,
            clusters: &[descriptor::CLUSTER, cluster_on_off::CLUSTER],
        },
    ],
};

fn handler(matter: &'static Matter<'static>) -> impl Metadata + NonBlockingHandler + 'static {
    (
        NODE,
        root_endpoint::handler(0, matter)
            .chain(
                1,
                descriptor::ID,
                descriptor::DescriptorCluster::new(*matter.borrow()),
            )
            .chain(
                1,
                cluster_on_off::ID,
                cluster_on_off::OnOffCluster::new(*matter.borrow()),
            ),
    )
}

#[allow(clippy::too_many_arguments)]
async fn create_ethernet_device(
    spi1: SPI1,
    dma0: DMA_CH0,
    dma1: DMA_CH1,
    miso: PIN_8,
    mosi: PIN_11,
    cs: PIN_9,
    clk: PIN_10,
    int: PIN_12,
    rst: PIN_13,
    spawner: &mut Spawner,
) -> Device<'static> {
    let mut spi_cfg = SpiConfig::default();
    spi_cfg.frequency = 50_000_000;

    let spi = Spi::new(spi1, clk, mosi, miso, dma0, dma1, spi_cfg);
    let cs = Output::new(cs, Level::High);
    let w5500_int = Input::new(int, Pull::Up);
    let w5500_reset = Output::new(rst, Level::High);

    let mac_addr = [0x02, 0x00, 0x00, 0x00, 0x00, 0x00];
    let state = make_static!(State::<8, 8>::new());
    let (device, runner) = embassy_net_w5500::new(
        mac_addr,
        state,
        ExclusiveDevice::new(spi, cs, Delay),
        w5500_int,
        w5500_reset,
    )
    .await;

    info!("About to start Ethernet");

    spawner.spawn(ethernet_task(runner)).unwrap();

    device
}

async fn create_ip_stack(
    device: Device<'static>,
    spawner: &mut Spawner,
) -> &'static Stack<Device<'static>> {
    let mut rng = RoscRng;

    // Generate random seed
    let seed = rng.next_u64();

    // Init network stack
    let stack = &*make_static!(Stack::new(
        device,
        embassy_net::Config {
            ipv4: embassy_net::ConfigV4::Dhcp(Default::default()),
            ipv6: embassy_net::ConfigV6::Static(embassy_net::StaticConfigV6 {
                address: Ipv6Cidr::new(
                    Ipv6Address::new(0xfe80, 0, 0, 0, 0x36b4, 0x72ff, 0xfe4c, 0x4410),
                    64
                ),
                gateway: Some(Ipv6Address::new(0xfe80, 0, 0, 0, 0, 0, 0, 0)),
                dns_servers: Default::default(),
            }),
        },
        make_static!(StackResources::<8>::new()),
        seed
    ));

    // Launch
    spawner.spawn(net_task(stack)).unwrap();

    stack
}

async fn wait_for_ip_config(
    stack: &'static Stack<Device<'static>>,
) -> (embassy_net::StaticConfigV4, embassy_net::StaticConfigV6) {
    info!("Waiting for IP config...");

    let config_ipv4 = loop {
        if let Some(config) = stack.config_v4() {
            break config.clone();
        }
        yield_now().await;
    };

    let config_ipv6 = loop {
        if let Some(config) = stack.config_v6() {
            break config.clone();
        }
        yield_now().await;
    };

    (config_ipv4, config_ipv6)
}

// Initialize the allocator
// Only necessary for a handful of Rust crypto crates
fn init_allocator() {
    use core::mem::MaybeUninit;
    const HEAP_SIZE: usize = 10 * 1024;

    static mut HEAP_MEM: [MaybeUninit<u8>; HEAP_SIZE] = [MaybeUninit::uninit(); HEAP_SIZE];
    unsafe { HEAP.init(HEAP_MEM.as_ptr() as usize, HEAP_SIZE) }
}

fn epoch() -> core::time::Duration {
    core::time::Duration::from_millis(embassy_time::Instant::now().as_millis())
}

fn rand(buf: &mut [u8]) {
    let mut rng = RoscRng;

    rng.fill_bytes(buf);
}

#[panic_handler]
fn panic(info: &core::panic::PanicInfo) -> ! {
    error!("PANIC!: {}", info);

    loop {}
}
