#![no_std]
#![no_main]

use sx126x::conf::Config as LoRaConfig;
use sx126x::op::status::CommandStatus::{CommandTimeout, CommandTxDone, DataAvailable};
use sx126x::op::*;
use sx126x::SX126x;

use core::mem::MaybeUninit;
use core::sync::atomic::{AtomicBool, Ordering};

use cortex_m_rt::entry;
use cortex_m_semihosting::{hprint, hprintln};
use embedded_hal::digital::v2::OutputPin;
use panic_semihosting as _;
use stm32f1xx_hal::delay::Delay;
use stm32f1xx_hal::gpio::*;
use stm32f1xx_hal::prelude::*;
use stm32f1xx_hal::spi::{Mode as SpiMode, Phase, Polarity, Spi};
use stm32f1xx_hal::stm32;

use stm32::interrupt;

type Dio1Pin = gpiob::PB10<Input<Floating>>;

const RF_FREQUENCY: u32 = 868_000_000; // 868MHz (EU)
const F_XTAL: u32 = 32_000_000; // 32MHz

/// Static reference to the DIO1 pin, for use in the ISR EXTi15_10
static mut DIO1_PIN: MaybeUninit<Dio1Pin> = MaybeUninit::uninit();

/// Flag which is raised when DIO1 level was risen.
static DIO1_RISEN: AtomicBool = AtomicBool::new(false);
#[interrupt]
fn EXTI15_10() {
    let dio1_pin = unsafe { &mut *DIO1_PIN.as_mut_ptr() };
    // Check whether the interrupt was caused by the DIO1 pin
    if dio1_pin.check_interrupt() {
        DIO1_RISEN.store(true, Ordering::Relaxed);

        // if we don't clear this bit, the ISR would trigger indefinitely
        dio1_pin.clear_interrupt_pending_bit();
    }
}

#[entry]
fn main() -> ! {
    let peripherals = stm32::Peripherals::take().unwrap();
    let core_peripherals = stm32::CorePeripherals::take().unwrap();

    let mut rcc = peripherals.RCC.constrain();
    let mut flash = peripherals.FLASH.constrain();
    let mut afio = peripherals.AFIO.constrain(&mut rcc.apb2);
    let exti = peripherals.EXTI;

    let clocks = rcc.cfgr.freeze(&mut flash.acr);

    let mut gpioa = peripherals.GPIOA.split(&mut rcc.apb2);
    let mut gpiob = peripherals.GPIOB.split(&mut rcc.apb2);

    // ===== Init SPI1 =====
    let spi1_sck = gpioa.pa5.into_alternate_push_pull(&mut gpioa.crl);
    let spi1_miso = gpioa.pa6.into_floating_input(&mut gpioa.crl);
    let spi1_mosi = gpioa.pa7.into_alternate_push_pull(&mut gpioa.crl);
    let spi1_mode = SpiMode {
        polarity: Polarity::IdleLow,
        phase: Phase::CaptureOnFirstTransition,
    };
    let spi1_freq = 100.khz();

    let spi1_pins = (
        spi1_sck,  // D13
        spi1_miso, // D12
        spi1_mosi, // D11
    );

    let spi1 = &mut Spi::spi1(
        peripherals.SPI1,
        spi1_pins,
        &mut afio.mapr,
        spi1_mode,
        spi1_freq,
        clocks,
        &mut rcc.apb2,
    );

    // ===== Init pins =====
    let lora_nreset = gpioa
        .pa0
        .into_push_pull_output_with_state(&mut gpioa.crl, State::High);
    let lora_nss = gpioa
        .pa8
        .into_push_pull_output_with_state(&mut gpioa.crh, State::High);
    let lora_busy = gpiob.pb5.into_floating_input(&mut gpiob.crl);
    let lora_ant = gpioa
        .pa9
        .into_push_pull_output_with_state(&mut gpioa.crh, State::High);

    // This is safe as long as the only other place we use DIO1_PIN is in the ISR
    let lora_dio1 = unsafe { &mut *DIO1_PIN.as_mut_ptr() };

    // Configure dio1 pin as interrupt source and store a reference to it in
    // DIO1_PIN static
    *lora_dio1 = gpiob.pb10.into_floating_input(&mut gpiob.crh);
    lora_dio1.make_interrupt_source(&mut afio);
    lora_dio1.trigger_on_edge(&exti, Edge::RISING);
    lora_dio1.enable_interrupt(&exti);
    // Wrap DIO1 pin in Dio1PinRefMut newtype, as mutable refences to
    // pins do not implement the `embedded_hal::digital::v2::InputPin` trait.
    let mut lora_dio1 = Dio1PinRefMut(lora_dio1);

    let lora_pins = (
        lora_nss,    // D7
        lora_nreset, // A0
        lora_busy,   // D4
        lora_ant,    // D8
    );

    // Initialize a busy-waiting delay based on the system clock
    let delay = &mut Delay::new(core_peripherals.SYST, clocks);

    // ===== Init LoRa modem =====
    let conf = build_config();
    let mut lora = SX126x::new(lora_pins);
    lora.init(spi1, delay, conf).unwrap();

    let tx_timeout = 0.into(); // TX timeout disabled
    let rx_timeout = RxTxTimeout::from_ms(3000);
    let crc_type = packet::lora::LoRaCrcType::CrcOn;

    let mut led_pin = gpioa
        .pa10
        .into_push_pull_output_with_state(&mut gpioa.crh, State::Low);

    // Unmask the EXTI15_10 ISR in the NVIC register
    unsafe {
        stm32::NVIC::unmask(stm32::Interrupt::EXTI15_10);
    }

    // Set the device in receiving mode
    lora.set_rx(spi1, delay, rx_timeout).unwrap();
    loop {
        if DIO1_RISEN.swap(false, Ordering::Relaxed) {
            lora.clear_irq_status(spi1, delay, IrqMask::all()).unwrap();
            let status = lora.get_status(spi1, delay).unwrap();
            match status.command_status() {
                Some(DataAvailable) => {
                    led_pin.set_high().unwrap();
                    // Get payload length and start offset in rx buffer
                    let buffer_status = lora.get_rx_buffer_status(spi1, delay).unwrap();
                    let payload_len = buffer_status.payload_length_rx();
                    let start_offset = buffer_status.rx_start_buffer_pointer();

                    hprint!("Message {}, {}: \"", start_offset, payload_len).unwrap();
                    // Read the received message in chunks of 32 bytes
                    let mut chunk_result = [0u8; 32];
                    for i in (0..payload_len).step_by(chunk_result.len()) {
                        let end = (payload_len - i).min(chunk_result.len() as u8) as usize;
                        lora.read_buffer(spi1, delay, i + start_offset, &mut chunk_result[..end])
                            .unwrap();
                        hprint!("{}", unsafe {
                            core::str::from_utf8_unchecked(&chunk_result)
                        })
                        .unwrap();
                    }
                    hprintln!("\"").unwrap();

                    // Send a message back
                    lora.write_bytes(
                        spi1,
                        delay,
                        b"Hello from sx126x-rs!",
                        tx_timeout,
                        8,
                        crc_type,
                        &mut lora_dio1,
                    )
                    .unwrap();
                    led_pin.set_low().unwrap();
                }

                Some(CommandTxDone) => {
                    // Set the device in Rx mode when done sending response message
                    lora.set_rx(spi1, delay, rx_timeout).unwrap();
                }
                Some(CommandTimeout) => hprintln!("CommandTimeout").unwrap(),
                other => hprintln!("Other: {:?}", other).unwrap(),
            }
        }
    }
}

fn build_config() -> LoRaConfig {
    use sx126x::op::{
        irq::IrqMaskBit::*, modulation::lora::*, packet::lora::LoRaPacketParams,
        rxtx::DeviceSel::SX1261, PacketType::LoRa,
    };

    let mod_params = LoraModParams::default().into();
    let tx_params = TxParams::default()
        .set_power_dbm(14)
        .set_ramp_time(RampTime::Ramp200u);
    let pa_config = PaConfig::default()
        .set_device_sel(SX1261)
        .set_pa_duty_cycle(0x04);

    let dio1_irq_mask = IrqMask::none()
        .combine(TxDone)
        .combine(Timeout)
        .combine(RxDone);

    let packet_params = LoRaPacketParams::default().into();

    let rf_freq = sx126x::calc_rf_freq(RF_FREQUENCY as f32, F_XTAL as f32);

    LoRaConfig {
        packet_type: LoRa,
        sync_word: 0x1424, // Private networks
        calib_param: CalibParam::from(0x7F),
        mod_params,
        tx_params,
        pa_config,
        packet_params: Some(packet_params),
        dio1_irq_mask,
        dio2_irq_mask: IrqMask::none(),
        dio3_irq_mask: IrqMask::none(),
        rf_frequency: RF_FREQUENCY,
        rf_freq,
    }
}

/// Wraps a mutable reference to a Dio1Pin,
/// so we can implement `embedded_hal::digital::v2::InputPin` for it,
/// which is necessary in order to pass it to the SX126x driver.
struct Dio1PinRefMut<'dio1>(&'dio1 mut Dio1Pin);

impl<'dio1> embedded_hal::digital::v2::InputPin for Dio1PinRefMut<'dio1> {
    type Error = core::convert::Infallible;

    fn is_high(&self) -> Result<bool, Self::Error> {
        self.0.is_high()
    }

    fn is_low(&self) -> Result<bool, Self::Error> {
        self.0.is_low()
    }
}
