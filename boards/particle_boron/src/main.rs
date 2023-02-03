//! Tock kernel for the Particle Boron.
//!
//! It is based on nRF52840 SoC (Cortex M4 core with a BLE transceiver) with
//! many exported I/O and peripherals.

#![no_std]
// Disable this attribute when documenting, as a workaround for
// https://github.com/rust-lang/rust/issues/62184.
#![cfg_attr(not(doc), no_main)]
#![deny(missing_docs)]

use capsules_core::i2c_master_slave_driver::I2CMasterSlaveDriver;
use capsules_core::virtualizers::virtual_aes_ccm::MuxAES128CCM;
use capsules_core::virtualizers::virtual_alarm::VirtualMuxAlarm;
use kernel::component::Component;
use kernel::deferred_call::DeferredCallClient;
use kernel::hil::i2c::{I2CMaster, I2CSlave};
use kernel::hil::led::LedLow;
use kernel::hil::symmetric_encryption::AES128;
use kernel::hil::time::Counter;
use kernel::hil::usb::Client;
use kernel::platform::{KernelResources, SyscallDriverLookup};
use kernel::scheduler::round_robin::RoundRobinSched;
#[allow(unused_imports)]
use kernel::{capabilities, create_capability, debug, debug_gpio, debug_verbose, static_init};
use nrf52840::gpio::Pin;
use nrf52840::interrupt_service::Nrf52840DefaultPeripherals;
#[allow(unused_imports)]
use nrf52_components::{self, UartChannel, UartPins};

// The Particle Boron LEDs
const LED_USR_PIN: Pin = Pin::P1_12;
const LED2_R_PIN: Pin = Pin::P0_13;
const LED2_G_PIN: Pin = Pin::P0_14;
const LED2_B_PIN: Pin = Pin::P0_15;

// The Particle Boron buttons
const BUTTON_PIN: Pin = Pin::P0_11;
const BUTTON_RST_PIN: Pin = Pin::P0_18;

// UART Pins
const _UART_RTS: Option<Pin> = Some(Pin::P0_30);
const _UART_TXD: Pin = Pin::P0_06;
const _UART_CTS: Option<Pin> = Some(Pin::P0_31);
const _UART_RXD: Pin = Pin::P0_08;

// SPI pins not currently in use, but left here for convenience
const _SPI_MOSI: Pin = Pin::P1_13;
const _SPI_MISO: Pin = Pin::P1_14;
const _SPI_CLK: Pin = Pin::P1_15;

// I2C Pins
const I2C_SDA_PIN: Pin = Pin::P0_26;
const I2C_SCL_PIN: Pin = Pin::P0_27;

// Constants related to the configuration of the 15.4 network stack
const SRC_MAC: u16 = 0xf00f;
const PAN_ID: u16 = 0xABCD;

/// UART Writer
pub mod io;

// How should the kernel respond when a process faults. For this board we choose
// to stop the app and print a notice, but not immediately panic. This allows
// users to debug their apps, but avoids issues with using the USB/CDC stack
// synchronously for panic! too early after the board boots.
const FAULT_RESPONSE: kernel::process::StopWithDebugFaultPolicy =
    kernel::process::StopWithDebugFaultPolicy {};

// Number of concurrent processes this platform supports.
const NUM_PROCS: usize = 8;

static mut PROCESSES: [Option<&'static dyn kernel::process::Process>; NUM_PROCS] =
    [None; NUM_PROCS];

// Static reference to chip for panic dumps
static mut CHIP: Option<&'static nrf52840::chip::NRF52<Nrf52840DefaultPeripherals>> = None;
// Static reference to process printer for panic dumps
static mut PROCESS_PRINTER: Option<&'static kernel::process::ProcessPrinterText> = None;
static mut CDC_REF_FOR_PANIC: Option<
    &'static capsules_extra::usb::cdc::CdcAcm<
        'static,
        nrf52::usbd::Usbd,
        capsules_core::virtualizers::virtual_alarm::VirtualMuxAlarm<'static, nrf52::rtc::Rtc>,
    >,
> = None;
static mut NRF52_POWER: Option<&'static nrf52840::power::Power> = None;

/// Dummy buffer that causes the linker to reserve enough space for the stack.
#[no_mangle]
#[link_section = ".stack_buffer"]
pub static mut STACK_MEMORY: [u8; 0x1000] = [0; 0x1000];

// Function for the process console to use to reboot the board
fn reset() -> ! {
    unsafe {
        cortexm4::scb::reset();
    }
    loop {
        cortexm4::support::nop();
    }
}

/// Supported drivers by the platform
pub struct Platform {
    ble_radio: &'static capsules_extra::ble_advertising_driver::BLE<
        'static,
        nrf52840::ble_radio::Radio<'static>,
        VirtualMuxAlarm<'static, nrf52840::rtc::Rtc<'static>>,
    >,
    ieee802154_radio: &'static capsules_extra::ieee802154::RadioDriver<'static>,
    button: &'static capsules_core::button::Button<'static, nrf52840::gpio::GPIOPin<'static>>,
    pconsole: &'static capsules_core::process_console::ProcessConsole<
        'static,
        { capsules_core::process_console::DEFAULT_COMMAND_HISTORY_LEN },
        VirtualMuxAlarm<'static, nrf52840::rtc::Rtc<'static>>,
        components::process_console::Capability,
    >,
    console: &'static capsules_core::console::Console<'static>,
    gpio: &'static capsules_core::gpio::GPIO<'static, nrf52840::gpio::GPIOPin<'static>>,
    led: &'static capsules_core::led::LedDriver<
        'static,
        LedLow<'static, nrf52840::gpio::GPIOPin<'static>>,
        4,
    >,
    adc: &'static capsules_core::adc::AdcVirtualized<'static>,
    rng: &'static capsules_core::rng::RngDriver<'static>,
    temp: &'static capsules_extra::temperature::TemperatureSensor<'static>,
    ipc: kernel::ipc::IPC<{ NUM_PROCS as u8 }>,
    i2c_master_slave: &'static capsules_core::i2c_master_slave_driver::I2CMasterSlaveDriver<
        'static,
        nrf52840::i2c::TWI,
    >,
    alarm: &'static capsules_core::alarm::AlarmDriver<
        'static,
        capsules_core::virtualizers::virtual_alarm::VirtualMuxAlarm<
            'static,
            nrf52840::rtc::Rtc<'static>,
        >,
    >,
    scheduler: &'static RoundRobinSched<'static>,
    systick: cortexm4::systick::SysTick,
}

impl SyscallDriverLookup for Platform {
    fn with_driver<F, R>(&self, driver_num: usize, f: F) -> R
    where
        F: FnOnce(Option<&dyn kernel::syscall::SyscallDriver>) -> R,
    {
        match driver_num {
            capsules_core::console::DRIVER_NUM => f(Some(self.console)),
            capsules_core::gpio::DRIVER_NUM => f(Some(self.gpio)),
            capsules_core::alarm::DRIVER_NUM => f(Some(self.alarm)),
            capsules_core::led::DRIVER_NUM => f(Some(self.led)),
            capsules_core::button::DRIVER_NUM => f(Some(self.button)),
            capsules_core::adc::DRIVER_NUM => f(Some(self.adc)),
            capsules_core::rng::DRIVER_NUM => f(Some(self.rng)),
            capsules_extra::ble_advertising_driver::DRIVER_NUM => f(Some(self.ble_radio)),
            capsules_extra::ieee802154::DRIVER_NUM => f(Some(self.ieee802154_radio)),
            capsules_extra::temperature::DRIVER_NUM => f(Some(self.temp)),
            kernel::ipc::DRIVER_NUM => f(Some(&self.ipc)),
            capsules_core::i2c_master_slave_driver::DRIVER_NUM => f(Some(self.i2c_master_slave)),
            _ => f(None),
        }
    }
}

impl KernelResources<nrf52840::chip::NRF52<'static, Nrf52840DefaultPeripherals<'static>>>
    for Platform
{
    type SyscallDriverLookup = Self;
    type SyscallFilter = ();
    type ProcessFault = ();
    type CredentialsCheckingPolicy = ();
    type Scheduler = RoundRobinSched<'static>;
    type SchedulerTimer = cortexm4::systick::SysTick;
    type WatchDog = ();
    type ContextSwitchCallback = ();

    fn syscall_driver_lookup(&self) -> &Self::SyscallDriverLookup {
        &self
    }
    fn syscall_filter(&self) -> &Self::SyscallFilter {
        &()
    }
    fn process_fault(&self) -> &Self::ProcessFault {
        &()
    }
    fn credentials_checking_policy(&self) -> &'static Self::CredentialsCheckingPolicy {
        &()
    }
    fn scheduler(&self) -> &Self::Scheduler {
        self.scheduler
    }
    fn scheduler_timer(&self) -> &Self::SchedulerTimer {
        &self.systick
    }
    fn watchdog(&self) -> &Self::WatchDog {
        &()
    }
    fn context_switch_callback(&self) -> &Self::ContextSwitchCallback {
        &()
    }
}

/// This is in a separate, inline(never) function so that its stack frame is
/// removed when this function returns. Otherwise, the stack space used for
/// these static_inits is wasted.
#[inline(never)]
unsafe fn create_peripherals() -> &'static mut Nrf52840DefaultPeripherals<'static> {
    // Initialize chip peripheral drivers
    let nrf52840_peripherals = static_init!(
        Nrf52840DefaultPeripherals,
        Nrf52840DefaultPeripherals::new()
    );

    nrf52840_peripherals
}

/// Main function called after RAM initialized.
#[no_mangle]
pub unsafe fn main() {
    nrf52840::init();

    let nrf52840_peripherals = create_peripherals();

    // set up circular peripheral dependencies
    nrf52840_peripherals.init();
    let base_peripherals = &nrf52840_peripherals.nrf52;

    // Save a reference to the power module for resetting the board into the
    // bootloader.
    NRF52_POWER = Some(&base_peripherals.pwr_clk);

    let board_kernel = static_init!(kernel::Kernel, kernel::Kernel::new(&PROCESSES));

    //--------------------------------------------------------------------------
    // CAPABILITIES
    //--------------------------------------------------------------------------

    // Create capabilities that the board needs to call certain protected kernel
    // functions.
    let process_management_capability =
        create_capability!(capabilities::ProcessManagementCapability);
    let main_loop_capability = create_capability!(capabilities::MainLoopCapability);
    let memory_allocation_capability = create_capability!(capabilities::MemoryAllocationCapability);

    //--------------------------------------------------------------------------
    // DEBUG GPIO
    //--------------------------------------------------------------------------

    let gpio_port = &nrf52840_peripherals.gpio_port;
    // Configure kernel debug GPIOs as early as possible. These are used by the
    // `debug_gpio!(0, toggle)` macro. We configure these early so that the
    // macro is available during most of the setup code and kernel execution.
    kernel::debug::assign_gpios(Some(&gpio_port[LED2_R_PIN]), None, None);

    //--------------------------------------------------------------------------
    // GPIO
    //--------------------------------------------------------------------------

    let gpio = components::gpio::GpioComponent::new(
        board_kernel,
        capsules_core::gpio::DRIVER_NUM,
        components::gpio_component_helper!(
            nrf52840::gpio::GPIOPin,
            // Left Side pins on mesh feather
            // A0 - ADC
            // 0 => &nrf52840_peripherals.gpio_port[Pin::P0_03],
            // A1 - ADC
            // 1 => &nrf52840_peripherals.gpio_port[Pin::P0_04],
            // A2 - ADC
            // 2 => &nrf52840_peripherals.gpio_port[Pin::P0_28],
            // A3 - ADC
            // 3 => &nrf52840_peripherals.gpio_port[Pin::P0_29],
            // A4 - ADC
            // 4 => &nrf52840_peripherals.gpio_port[Pin::P0_30],
            // A5 - ADC
            // 5 => &nrf52840_peripherals.gpio_port[Pin::P0_31],
            //D13
            6 => &nrf52840_peripherals.gpio_port[Pin::P1_15],
            //D12
            7 => &nrf52840_peripherals.gpio_port[Pin::P1_13],
            //D11
            8 => &nrf52840_peripherals.gpio_port[Pin::P1_14],
            //D10
            9 => &nrf52840_peripherals.gpio_port[Pin::P0_08],
            //D9
            10 => &nrf52840_peripherals.gpio_port[Pin::P0_06],
            // Right Side pins on mesh feather
            //D8
            11 => &nrf52840_peripherals.gpio_port[Pin::P1_03],
            //D7: Bound to LED_USR_PIN (Active Low)
            12 => &nrf52840_peripherals.gpio_port[Pin::P1_12],
            //D6
            13 => &nrf52840_peripherals.gpio_port[Pin::P1_11],
            //D5
            14 => &nrf52840_peripherals.gpio_port[Pin::P1_10],
            //D4
            15 => &nrf52840_peripherals.gpio_port[Pin::P1_08],
            //D3
            16 => &nrf52840_peripherals.gpio_port[Pin::P1_02],
            //D2
            17 => &nrf52840_peripherals.gpio_port[Pin::P0_01],
            //D1
            18 => &nrf52840_peripherals.gpio_port[Pin::P0_27],
            //D0
            19 => &nrf52840_peripherals.gpio_port[Pin::P0_26],
        ),
    )
    .finalize(components::gpio_component_static!(nrf52840::gpio::GPIOPin));

    //--------------------------------------------------------------------------
    // Buttons
    //--------------------------------------------------------------------------

    let button = components::button::ButtonComponent::new(
        board_kernel,
        capsules_core::button::DRIVER_NUM,
        components::button_component_helper!(
            nrf52840::gpio::GPIOPin,
            (
                &nrf52840_peripherals.gpio_port[BUTTON_PIN],
                kernel::hil::gpio::ActivationMode::ActiveLow,
                kernel::hil::gpio::FloatingState::PullUp
            )
        ),
    )
    .finalize(components::button_component_static!(
        nrf52840::gpio::GPIOPin
    ));

    //--------------------------------------------------------------------------
    // LEDs
    //--------------------------------------------------------------------------

    let led = components::led::LedsComponent::new().finalize(components::led_component_static!(
        LedLow<'static, nrf52840::gpio::GPIOPin>,
        LedLow::new(&nrf52840_peripherals.gpio_port[LED_USR_PIN]),
        LedLow::new(&nrf52840_peripherals.gpio_port[LED2_R_PIN]),
        LedLow::new(&nrf52840_peripherals.gpio_port[LED2_G_PIN]),
        LedLow::new(&nrf52840_peripherals.gpio_port[LED2_B_PIN]),
    ));

    nrf52_components::startup::NrfStartupComponent::new(
        false,
        BUTTON_RST_PIN,
        nrf52840::uicr::Regulator0Output::V3_0,
        &base_peripherals.nvmc,
    )
    .finalize(());

    //--------------------------------------------------------------------------
    // ALARM & TIMER
    //--------------------------------------------------------------------------

    let rtc = &base_peripherals.rtc;
    let _ = rtc.start();
    let mux_alarm = components::alarm::AlarmMuxComponent::new(rtc)
        .finalize(components::alarm_mux_component_static!(nrf52840::rtc::Rtc));
    let alarm = components::alarm::AlarmDriverComponent::new(
        board_kernel,
        capsules_core::alarm::DRIVER_NUM,
        mux_alarm,
    )
    .finalize(components::alarm_component_static!(nrf52840::rtc::Rtc));

    //--------------------------------------------------------------------------
    // UART & CONSOLE & DEBUG
    //--------------------------------------------------------------------------

    // Setup the CDC-ACM over USB driver that we will use for UART.
    // We use the Arduino Vendor ID and Product ID since the device is the same.

    // Create the strings we include in the USB descriptor. We use the hardcoded
    // DEVICEADDR register on the nRF52 to set the serial number.
    let serial_number_buf = static_init!([u8; 17], [0; 17]);
    let serial_number_string: &'static str =
        nrf52::ficr::FICR_INSTANCE.address_str(serial_number_buf);
    let strings = static_init!(
        [&str; 3],
        [
            "Particle",           // Manufacturer
            "Boron - TockOS",     // Product
            serial_number_string, // Serial number
        ]
    );

    let cdc = components::cdc::CdcAcmComponent::new(
        &nrf52840_peripherals.usbd,
        capsules_extra::usb::cdc::MAX_CTRL_PACKET_SIZE_NRF52840,
        0x0101, // Custom
        0x0202, // Custom
        strings,
        mux_alarm,
        None,
    )
    .finalize(components::cdc_acm_component_static!(
        nrf52::usbd::Usbd,
        nrf52::rtc::Rtc
    ));
    CDC_REF_FOR_PANIC = Some(cdc); //for use by panic handler

    // Process Printer for displaying process information.
    let process_printer = components::process_printer::ProcessPrinterTextComponent::new()
        .finalize(components::process_printer_text_component_static!());
    PROCESS_PRINTER = Some(process_printer);

    // Create a shared UART channel for the console and for kernel debug.
    let uart_mux = components::console::UartMuxComponent::new(cdc, 115200)
        .finalize(components::uart_mux_component_static!());

    let pconsole = components::process_console::ProcessConsoleComponent::new(
        board_kernel,
        uart_mux,
        mux_alarm,
        process_printer,
        Some(reset),
    )
    .finalize(components::process_console_component_static!(
        nrf52::rtc::Rtc<'static>
    ));

    // Setup the console.
    let console = components::console::ConsoleComponent::new(
        board_kernel,
        capsules_core::console::DRIVER_NUM,
        uart_mux,
    )
    .finalize(components::console_component_static!());
    // Create the debugger object that handles calls to `debug!()`.
    components::debug_writer::DebugWriterComponent::new(uart_mux)
        .finalize(components::debug_writer_component_static!());

    //--------------------------------------------------------------------------
    // WIRELESS
    //--------------------------------------------------------------------------

    let ble_radio = components::ble::BLEComponent::new(
        board_kernel,
        capsules_extra::ble_advertising_driver::DRIVER_NUM,
        &base_peripherals.ble_radio,
        mux_alarm,
    )
    .finalize(components::ble_component_static!(
        nrf52840::rtc::Rtc,
        nrf52840::ble_radio::Radio
    ));

    let aes_mux = static_init!(
        MuxAES128CCM<'static, nrf52840::aes::AesECB>,
        MuxAES128CCM::new(&base_peripherals.ecb,)
    );
    base_peripherals.ecb.set_client(aes_mux);
    aes_mux.register();

    let (ieee802154_radio, _mux_mac) = components::ieee802154::Ieee802154Component::new(
        board_kernel,
        capsules_extra::ieee802154::DRIVER_NUM,
        &base_peripherals.ieee802154_radio,
        aes_mux,
        PAN_ID,
        SRC_MAC,
    )
    .finalize(components::ieee802154_component_static!(
        nrf52840::ieee802154_radio::Radio,
        nrf52840::aes::AesECB<'static>
    ));

    //--------------------------------------------------------------------------
    // Sensor
    //--------------------------------------------------------------------------

    let temp = components::temperature::TemperatureComponent::new(
        board_kernel,
        capsules_extra::temperature::DRIVER_NUM,
        &base_peripherals.temp,
    )
    .finalize(components::temperature_component_static!());

    //--------------------------------------------------------------------------
    // RANDOM NUMBERS
    //--------------------------------------------------------------------------

    let rng = components::rng::RngComponent::new(
        board_kernel,
        capsules_core::rng::DRIVER_NUM,
        &base_peripherals.trng,
    )
    .finalize(components::rng_component_static!());

    //--------------------------------------------------------------------------
    // ADC
    //--------------------------------------------------------------------------

    base_peripherals.adc.calibrate();

    let adc_mux = components::adc::AdcMuxComponent::new(&base_peripherals.adc)
        .finalize(components::adc_mux_component_static!(nrf52840::adc::Adc));

    let adc_syscall =
        components::adc::AdcVirtualComponent::new(board_kernel, capsules_core::adc::DRIVER_NUM)
            .finalize(components::adc_syscall_component_helper!(
                // BRD_A0
                components::adc::AdcComponent::new(
                    &adc_mux,
                    nrf52840::adc::AdcChannelSetup::new(nrf52840::adc::AdcChannel::AnalogInput1)
                )
                .finalize(components::adc_component_static!(nrf52840::adc::Adc)),
                // BRD_A1
                components::adc::AdcComponent::new(
                    &adc_mux,
                    nrf52840::adc::AdcChannelSetup::new(nrf52840::adc::AdcChannel::AnalogInput2)
                )
                .finalize(components::adc_component_static!(nrf52840::adc::Adc)),
                // BRD_A2
                components::adc::AdcComponent::new(
                    &adc_mux,
                    nrf52840::adc::AdcChannelSetup::new(nrf52840::adc::AdcChannel::AnalogInput4)
                )
                .finalize(components::adc_component_static!(nrf52840::adc::Adc)),
                // BRD_A3
                components::adc::AdcComponent::new(
                    &adc_mux,
                    nrf52840::adc::AdcChannelSetup::new(nrf52840::adc::AdcChannel::AnalogInput5)
                )
                .finalize(components::adc_component_static!(nrf52840::adc::Adc)),
                // BRD_A4
                components::adc::AdcComponent::new(
                    &adc_mux,
                    nrf52840::adc::AdcChannelSetup::new(nrf52840::adc::AdcChannel::AnalogInput6)
                )
                .finalize(components::adc_component_static!(nrf52840::adc::Adc)),
                // BRD_A5
                components::adc::AdcComponent::new(
                    &adc_mux,
                    nrf52840::adc::AdcChannelSetup::new(nrf52840::adc::AdcChannel::AnalogInput7)
                )
                .finalize(components::adc_component_static!(nrf52840::adc::Adc)),
            ));

    //--------------------------------------------------------------------------
    // I2C Master/Slave
    //--------------------------------------------------------------------------

    let i2c_master_buffer = static_init!([u8; 32], [0; 32]);
    let i2c_slave_buffer1 = static_init!([u8; 32], [0; 32]);
    let i2c_slave_buffer2 = static_init!([u8; 32], [0; 32]);

    let i2c_master_slave = static_init!(
        I2CMasterSlaveDriver<nrf52840::i2c::TWI>,
        I2CMasterSlaveDriver::new(
            &base_peripherals.twi1,
            i2c_master_buffer,
            i2c_slave_buffer1,
            i2c_slave_buffer2,
            board_kernel.create_grant(
                capsules_core::i2c_master_slave_driver::DRIVER_NUM,
                &memory_allocation_capability
            ),
        )
    );
    base_peripherals.twi1.configure(
        nrf52840::pinmux::Pinmux::new(I2C_SCL_PIN as u32),
        nrf52840::pinmux::Pinmux::new(I2C_SDA_PIN as u32),
    );
    base_peripherals.twi1.set_master_client(i2c_master_slave);
    base_peripherals.twi1.set_slave_client(i2c_master_slave);
    base_peripherals.twi1.set_speed(nrf52840::i2c::Speed::K400);

    //--------------------------------------------------------------------------
    // FINAL SETUP AND BOARD BOOT
    //--------------------------------------------------------------------------

    nrf52_components::NrfClockComponent::new(&base_peripherals.clock).finalize(());

    let scheduler = components::sched::round_robin::RoundRobinComponent::new(&PROCESSES)
        .finalize(components::round_robin_component_static!(NUM_PROCS));

    let platform = Platform {
        button,
        ble_radio,
        ieee802154_radio,
        pconsole,
        console,
        led,
        gpio,
        adc: adc_syscall,
        rng,
        temp,
        alarm,
        ipc: kernel::ipc::IPC::new(
            board_kernel,
            kernel::ipc::DRIVER_NUM,
            &memory_allocation_capability,
        ),
        i2c_master_slave,
        scheduler,
        systick: cortexm4::systick::SysTick::new_with_calibration(64000000),
    };

    let chip = static_init!(
        nrf52840::chip::NRF52<Nrf52840DefaultPeripherals>,
        nrf52840::chip::NRF52::new(nrf52840_peripherals)
    );
    CHIP = Some(chip);

    // Configure the USB stack to enable a serial port over CDC-ACM.
    cdc.enable();
    cdc.attach();

    debug!("Particle Boron: Initialization complete. Entering main loop\r");
    let _ = platform.pconsole.start();

    //--------------------------------------------------------------------------
    // PROCESSES AND MAIN LOOP
    //--------------------------------------------------------------------------

    // These symbols are defined in the linker script.
    extern "C" {
        /// Beginning of the ROM region containing app images.
        static _sapps: u8;
        /// End of the ROM region containing app images.
        static _eapps: u8;
        /// Beginning of the RAM region for app memory.
        static mut _sappmem: u8;
        /// End of the RAM region for app memory.
        static _eappmem: u8;
    }

    kernel::process::load_processes(
        board_kernel,
        chip,
        core::slice::from_raw_parts(
            &_sapps as *const u8,
            &_eapps as *const u8 as usize - &_sapps as *const u8 as usize,
        ),
        core::slice::from_raw_parts_mut(
            &mut _sappmem as *mut u8,
            &_eappmem as *const u8 as usize - &_sappmem as *const u8 as usize,
        ),
        &mut PROCESSES,
        &FAULT_RESPONSE,
        &process_management_capability,
    )
    .unwrap_or_else(|err| {
        debug!("Error loading processes!");
        debug!("{:?}", err);
    });

    board_kernel.kernel_loop(&platform, chip, Some(&platform.ipc), &main_loop_capability);
}
