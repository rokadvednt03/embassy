use core::marker::PhantomData;

use embassy_embedded_hal::SetConfig;
use embassy_hal_common::into_ref;

use crate::gpio::sealed::AFType;
use crate::gpio::Pull;
use crate::i2c::{Error, Instance, SclPin, SdaPin};
use crate::pac::i2c;
use crate::time::Hertz;
use crate::Peripheral;

#[non_exhaustive]
#[derive(Copy, Clone)]
pub struct Config {
    pub sda_pullup: bool,
    pub scl_pullup: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            sda_pullup: false,
            scl_pullup: false,
        }
    }
}

pub struct State {}

impl State {
    pub(crate) const fn new() -> Self {
        Self {}
    }
}

pub struct I2c<'d, T: Instance> {
    phantom: PhantomData<&'d mut T>,
}

impl<'d, T: Instance> I2c<'d, T> {
    pub fn new(
        _peri: impl Peripheral<P = T> + 'd,
        scl: impl Peripheral<P = impl SclPin<T>> + 'd,
        sda: impl Peripheral<P = impl SdaPin<T>> + 'd,
        freq: Hertz,
        config: Config,
    ) -> Self {
        into_ref!(scl, sda);

        T::enable();
        T::reset();

        unsafe {
            scl.set_as_af_pull(
                scl.af_num(),
                AFType::OutputOpenDrain,
                match config.scl_pullup {
                    true => Pull::Up,
                    false => Pull::None,
                },
            );
            sda.set_as_af_pull(
                sda.af_num(),
                AFType::OutputOpenDrain,
                match config.sda_pullup {
                    true => Pull::Up,
                    false => Pull::None,
                },
            );
        }

        unsafe {
            T::regs().cr1().modify(|reg| {
                reg.set_pe(false);
                //reg.set_anfoff(false);
            });
        }

        let timings = Timings::new(T::frequency(), freq.into());

        unsafe {
            T::regs().cr2().modify(|reg| {
                reg.set_freq(timings.freq);
            });
            T::regs().ccr().modify(|reg| {
                reg.set_f_s(timings.mode.f_s());
                reg.set_duty(timings.duty.duty());
                reg.set_ccr(timings.ccr);
            });
            T::regs().trise().modify(|reg| {
                reg.set_trise(timings.trise);
            });
        }

        unsafe {
            T::regs().cr1().modify(|reg| {
                reg.set_pe(true);
            });
        }

        Self { phantom: PhantomData }
    }

    unsafe fn check_and_clear_error_flags(&self) -> Result<i2c::regs::Sr1, Error> {
        // Note that flags should only be cleared once they have been registered. If flags are
        // cleared otherwise, there may be an inherent race condition and flags may be missed.
        let sr1 = T::regs().sr1().read();

        if sr1.timeout() {
            T::regs().sr1().modify(|reg| reg.set_timeout(false));
            return Err(Error::Timeout);
        }

        if sr1.pecerr() {
            T::regs().sr1().modify(|reg| reg.set_pecerr(false));
            return Err(Error::Crc);
        }

        if sr1.ovr() {
            T::regs().sr1().modify(|reg| reg.set_ovr(false));
            return Err(Error::Overrun);
        }

        if sr1.af() {
            T::regs().sr1().modify(|reg| reg.set_af(false));
            return Err(Error::Nack);
        }

        if sr1.arlo() {
            T::regs().sr1().modify(|reg| reg.set_arlo(false));
            return Err(Error::Arbitration);
        }

        // The errata indicates that BERR may be incorrectly detected. It recommends ignoring and
        // clearing the BERR bit instead.
        if sr1.berr() {
            T::regs().sr1().modify(|reg| reg.set_berr(false));
        }

        Ok(sr1)
    }

    unsafe fn write_bytes(&mut self, addr: u8, bytes: &[u8]) -> Result<(), Error> {
        // Send a START condition

        T::regs().cr1().modify(|reg| {
            reg.set_start(true);
        });

        // Wait until START condition was generated
        while !self.check_and_clear_error_flags()?.start() {}

        // Also wait until signalled we're master and everything is waiting for us
        while {
            self.check_and_clear_error_flags()?;

            let sr2 = T::regs().sr2().read();
            !sr2.msl() && !sr2.busy()
        } {}

        // Set up current address, we're trying to talk to
        T::regs().dr().write(|reg| reg.set_dr(addr << 1));

        // Wait until address was sent
        // Wait for the address to be acknowledged
        // Check for any I2C errors. If a NACK occurs, the ADDR bit will never be set.
        while !self.check_and_clear_error_flags()?.addr() {}

        // Clear condition by reading SR2
        let _ = T::regs().sr2().read();

        // Send bytes
        for c in bytes {
            self.send_byte(*c)?;
        }

        // Fallthrough is success
        Ok(())
    }

    unsafe fn send_byte(&self, byte: u8) -> Result<(), Error> {
        // Wait until we're ready for sending
        while {
            // Check for any I2C errors. If a NACK occurs, the ADDR bit will never be set.
            !self.check_and_clear_error_flags()?.txe()
        } {}

        // Push out a byte of data
        T::regs().dr().write(|reg| reg.set_dr(byte));

        // Wait until byte is transferred
        while {
            // Check for any potential error conditions.
            !self.check_and_clear_error_flags()?.btf()
        } {}

        Ok(())
    }

    unsafe fn recv_byte(&self) -> Result<u8, Error> {
        while {
            // Check for any potential error conditions.
            self.check_and_clear_error_flags()?;

            !T::regs().sr1().read().rxne()
        } {}

        let value = T::regs().dr().read().dr();
        Ok(value)
    }

    pub fn blocking_read(&mut self, addr: u8, buffer: &mut [u8]) -> Result<(), Error> {
        if let Some((last, buffer)) = buffer.split_last_mut() {
            // Send a START condition and set ACK bit
            unsafe {
                T::regs().cr1().modify(|reg| {
                    reg.set_start(true);
                    reg.set_ack(true);
                });
            }

            // Wait until START condition was generated
            while unsafe { !T::regs().sr1().read().start() } {}

            // Also wait until signalled we're master and everything is waiting for us
            while {
                let sr2 = unsafe { T::regs().sr2().read() };
                !sr2.msl() && !sr2.busy()
            } {}

            // Set up current address, we're trying to talk to
            unsafe { T::regs().dr().write(|reg| reg.set_dr((addr << 1) + 1)) }

            // Wait until address was sent
            // Wait for the address to be acknowledged
            while unsafe { !self.check_and_clear_error_flags()?.addr() } {}

            // Clear condition by reading SR2
            let _ = unsafe { T::regs().sr2().read() };

            // Receive bytes into buffer
            for c in buffer {
                *c = unsafe { self.recv_byte()? };
            }

            // Prepare to send NACK then STOP after next byte
            unsafe {
                T::regs().cr1().modify(|reg| {
                    reg.set_ack(false);
                    reg.set_stop(true);
                })
            }

            // Receive last byte
            *last = unsafe { self.recv_byte()? };

            // Wait for the STOP to be sent.
            while unsafe { T::regs().cr1().read().stop() } {}

            // Fallthrough is success
            Ok(())
        } else {
            Err(Error::Overrun)
        }
    }

    pub fn blocking_write(&mut self, addr: u8, bytes: &[u8]) -> Result<(), Error> {
        unsafe {
            self.write_bytes(addr, bytes)?;
            // Send a STOP condition
            T::regs().cr1().modify(|reg| reg.set_stop(true));
            // Wait for STOP condition to transmit.
            while T::regs().cr1().read().stop() {}
        };

        // Fallthrough is success
        Ok(())
    }

    pub fn blocking_write_read(&mut self, addr: u8, bytes: &[u8], buffer: &mut [u8]) -> Result<(), Error> {
        unsafe { self.write_bytes(addr, bytes)? };
        self.blocking_read(addr, buffer)?;

        Ok(())
    }
}

impl<'d, T: Instance> embedded_hal_02::blocking::i2c::Read for I2c<'d, T> {
    type Error = Error;

    fn read(&mut self, addr: u8, buffer: &mut [u8]) -> Result<(), Self::Error> {
        self.blocking_read(addr, buffer)
    }
}

impl<'d, T: Instance> embedded_hal_02::blocking::i2c::Write for I2c<'d, T> {
    type Error = Error;

    fn write(&mut self, addr: u8, bytes: &[u8]) -> Result<(), Self::Error> {
        self.blocking_write(addr, bytes)
    }
}

impl<'d, T: Instance> embedded_hal_02::blocking::i2c::WriteRead for I2c<'d, T> {
    type Error = Error;

    fn write_read(&mut self, addr: u8, bytes: &[u8], buffer: &mut [u8]) -> Result<(), Self::Error> {
        self.blocking_write_read(addr, bytes, buffer)
    }
}

#[cfg(feature = "unstable-traits")]
mod eh1 {
    use super::*;

    impl embedded_hal_1::i2c::Error for Error {
        fn kind(&self) -> embedded_hal_1::i2c::ErrorKind {
            match *self {
                Self::Bus => embedded_hal_1::i2c::ErrorKind::Bus,
                Self::Arbitration => embedded_hal_1::i2c::ErrorKind::ArbitrationLoss,
                Self::Nack => {
                    embedded_hal_1::i2c::ErrorKind::NoAcknowledge(embedded_hal_1::i2c::NoAcknowledgeSource::Unknown)
                }
                Self::Timeout => embedded_hal_1::i2c::ErrorKind::Other,
                Self::Crc => embedded_hal_1::i2c::ErrorKind::Other,
                Self::Overrun => embedded_hal_1::i2c::ErrorKind::Overrun,
                Self::ZeroLengthTransfer => embedded_hal_1::i2c::ErrorKind::Other,
            }
        }
    }

    impl<'d, T: Instance> embedded_hal_1::i2c::ErrorType for I2c<'d, T> {
        type Error = Error;
    }

    impl<'d, T: Instance> embedded_hal_1::i2c::I2c for I2c<'d, T> {
        fn read(&mut self, address: u8, buffer: &mut [u8]) -> Result<(), Self::Error> {
            self.blocking_read(address, buffer)
        }

        fn write(&mut self, address: u8, buffer: &[u8]) -> Result<(), Self::Error> {
            self.blocking_write(address, buffer)
        }

        fn write_iter<B>(&mut self, _address: u8, _bytes: B) -> Result<(), Self::Error>
        where
            B: IntoIterator<Item = u8>,
        {
            todo!();
        }

        fn write_iter_read<B>(&mut self, _address: u8, _bytes: B, _buffer: &mut [u8]) -> Result<(), Self::Error>
        where
            B: IntoIterator<Item = u8>,
        {
            todo!();
        }

        fn write_read(&mut self, address: u8, wr_buffer: &[u8], rd_buffer: &mut [u8]) -> Result<(), Self::Error> {
            self.blocking_write_read(address, wr_buffer, rd_buffer)
        }

        fn transaction<'a>(
            &mut self,
            _address: u8,
            _operations: &mut [embedded_hal_1::i2c::Operation<'a>],
        ) -> Result<(), Self::Error> {
            todo!();
        }

        fn transaction_iter<'a, O>(&mut self, _address: u8, _operations: O) -> Result<(), Self::Error>
        where
            O: IntoIterator<Item = embedded_hal_1::i2c::Operation<'a>>,
        {
            todo!();
        }
    }
}

enum Mode {
    Fast,
    Standard,
}

impl Mode {
    fn f_s(&self) -> i2c::vals::FS {
        match self {
            Mode::Fast => i2c::vals::FS::FAST,
            Mode::Standard => i2c::vals::FS::STANDARD,
        }
    }
}

enum Duty {
    Duty2_1,
    Duty16_9,
}

impl Duty {
    fn duty(&self) -> i2c::vals::Duty {
        match self {
            Duty::Duty2_1 => i2c::vals::Duty::DUTY2_1,
            Duty::Duty16_9 => i2c::vals::Duty::DUTY16_9,
        }
    }
}

struct Timings {
    freq: u8,
    mode: Mode,
    trise: u8,
    ccr: u16,
    duty: Duty,
}

impl Timings {
    fn new(i2cclk: Hertz, speed: Hertz) -> Self {
        // Calculate settings for I2C speed modes
        let speed = speed.0;
        let clock = i2cclk.0;
        let freq = clock / 1_000_000;
        assert!(freq >= 2 && freq <= 50);

        // Configure bus frequency into I2C peripheral
        //self.i2c.cr2.write(|w| unsafe { w.freq().bits(freq as u8) });

        let trise = if speed <= 100_000 {
            freq + 1
        } else {
            (freq * 300) / 1000 + 1
        };

        let mut ccr;
        let duty;
        let mode;

        // I2C clock control calculation
        if speed <= 100_000 {
            duty = Duty::Duty2_1;
            mode = Mode::Standard;
            ccr = {
                let ccr = clock / (speed * 2);
                if ccr < 4 {
                    4
                } else {
                    ccr
                }
            };
        } else {
            const DUTYCYCLE: u8 = 0;
            mode = Mode::Fast;
            if DUTYCYCLE == 0 {
                duty = Duty::Duty2_1;
                ccr = clock / (speed * 3);
                ccr = if ccr < 1 { 1 } else { ccr };

                // Set clock to fast mode with appropriate parameters for selected speed (2:1 duty cycle)
            } else {
                duty = Duty::Duty16_9;
                ccr = clock / (speed * 25);
                ccr = if ccr < 1 { 1 } else { ccr };

                // Set clock to fast mode with appropriate parameters for selected speed (16:9 duty cycle)
            }
        }

        Self {
            freq: freq as u8,
            trise: trise as u8,
            ccr: ccr as u16,
            duty,
            mode,
            //prescale: presc_reg,
            //scll,
            //sclh,
            //sdadel,
            //scldel,
        }
    }
}

impl<'d, T: Instance> SetConfig for I2c<'d, T> {
    type Config = Hertz;
    fn set_config(&mut self, config: &Self::Config) {
        let timings = Timings::new(T::frequency(), *config);
        unsafe {
            T::regs().cr2().modify(|reg| {
                reg.set_freq(timings.freq);
            });
            T::regs().ccr().modify(|reg| {
                reg.set_f_s(timings.mode.f_s());
                reg.set_duty(timings.duty.duty());
                reg.set_ccr(timings.ccr);
            });
            T::regs().trise().modify(|reg| {
                reg.set_trise(timings.trise);
            });
        }
    }
}
