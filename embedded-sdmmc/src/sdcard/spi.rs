//! # SD card access via SPI
//!
//! Implements the BlockDevice trait for an SD/MMC Protocol over SPI.
use core::cell::RefCell;

use crate::{Block, BlockCount, BlockDevice, BlockIdx};
use embedded_sdmmc_types::sdcard::*;

// ****************************************************************************
// Types and Implementations
// ****************************************************************************

/// Driver for an SD Card on an SPI bus.
///
/// Built from an [`SpiDevice`] implementation and a Chip Select pin.
///
/// Before talking to the SD Card, the caller needs to send 74 clocks cycles on
/// the SPI Clock line, at 400 kHz, with no chip-select asserted (or at least,
/// not the chip-select of the SD Card).
///
/// This kind of breaks the embedded-hal model, so how to do this is left to
/// the caller. You could drive the SpiBus directly, or use an SpiDevice with
/// a dummy chip-select pin. Or you could try just not doing the 74 clocks and
/// see if your card works anyway - some do, some don't.
///
/// All the APIs take `&self` - mutability is handled using an inner `RefCell`.
///
/// [`SpiDevice`]: embedded_hal::spi::SpiDevice
pub struct SdCard<SPI, DELAYER>
where
    SPI: embedded_hal::spi::SpiDevice<u8>,
    DELAYER: embedded_hal::delay::DelayNs,
{
    inner: RefCell<SdCardInner<SPI, DELAYER>>,
}

impl<SPI, DELAYER> SdCard<SPI, DELAYER>
where
    SPI: embedded_hal::spi::SpiDevice<u8>,
    DELAYER: embedded_hal::delay::DelayNs,
{
    /// Create a new SD/MMC Card driver using a raw SPI interface.
    ///
    /// The card will not be initialised at this time. Initialisation is
    /// deferred until a method is called on the object.
    ///
    /// Uses the default options.
    pub fn new(spi: SPI, delayer: DELAYER) -> Self {
        Self::new_with_options(spi, delayer, AcquireOpts::default())
    }

    /// Construct a new SD/MMC Card driver, using a raw SPI interface and the given options.
    ///
    /// See the docs of the [`SdCard`] struct for more information about
    /// how to construct the needed `SPI` and `CS` types.
    ///
    /// The card will not be initialised at this time. Initialisation is
    /// deferred until a method is called on the object.
    pub fn new_with_options(spi: SPI, delayer: DELAYER, options: AcquireOpts) -> Self {
        SdCard {
            inner: RefCell::new(SdCardInner {
                spi,
                delayer,
                card_type: None,
                options,
            }),
        }
    }

    /// Get a temporary borrow on the underlying SPI device.
    ///
    /// The given closure will be called exactly once, and will be passed a
    /// mutable reference to the underlying SPI object.
    ///
    /// Useful if you need to re-clock the SPI, but does not perform card
    /// initialisation.
    pub fn spi<T, F>(&self, func: F) -> T
    where
        F: FnOnce(&mut SPI) -> T,
    {
        let mut inner = self.inner.borrow_mut();
        func(&mut inner.spi)
    }

    /// Return the usable size of this SD card in bytes.
    ///
    /// This will trigger card (re-)initialisation.
    pub fn num_bytes(&self) -> Result<u64, Error> {
        let mut inner = self.inner.borrow_mut();
        inner.check_init()?;
        inner.num_bytes()
    }

    /// Can this card erase single blocks?
    ///
    /// This will trigger card (re-)initialisation.
    pub fn erase_single_block_enabled(&self) -> Result<bool, Error> {
        let mut inner = self.inner.borrow_mut();
        inner.check_init()?;
        inner.erase_single_block_enabled()
    }

    /// Mark the card as requiring a reset.
    ///
    /// The next operation will assume the card has been freshly inserted.
    pub fn mark_card_uninit(&self) {
        let mut inner = self.inner.borrow_mut();
        inner.card_type = None;
    }

    /// Get the card type.
    ///
    /// This will trigger card (re-)initialisation.
    pub fn get_card_type(&self) -> Option<CardType> {
        let mut inner = self.inner.borrow_mut();
        inner.check_init().ok()?;
        inner.card_type
    }

    /// Tell the driver the card has been initialised.
    ///
    /// This is here in case you were previously using the SD Card, and then a
    /// previous instance of this object got destroyed but you know for certain
    /// the SD Card remained powered up and initialised, and you'd just like to
    /// read/write to/from the card again without going through the
    /// initialisation sequence again.
    ///
    /// # Safety
    ///
    /// Only do this if the SD Card has actually been initialised. That is, if
    /// you have been through the card initialisation sequence as specified in
    /// the SD Card Specification by sending each appropriate command in turn,
    /// either manually or using another variable of this [`SdCard`]. The card
    /// must also be of the indicated type. Failure to uphold this will cause
    /// data corruption.
    pub unsafe fn mark_card_as_init(&self, card_type: CardType) {
        let mut inner = self.inner.borrow_mut();
        inner.card_type = Some(card_type);
    }
}

impl<SPI, DELAYER> BlockDevice for SdCard<SPI, DELAYER>
where
    SPI: embedded_hal::spi::SpiDevice<u8>,
    DELAYER: embedded_hal::delay::DelayNs,
{
    type Error = Error;

    /// Read one or more blocks, starting at the given block index.
    ///
    /// This will trigger card (re-)initialisation.
    fn read(&self, blocks: &mut [Block], start_block_idx: BlockIdx) -> Result<(), Self::Error> {
        let mut inner = self.inner.borrow_mut();
        crate::debug!("Read {} blocks @ {}", blocks.len(), start_block_idx.0,);
        inner.check_init()?;
        inner.read(blocks, start_block_idx)
    }

    /// Write one or more blocks, starting at the given block index.
    ///
    /// This will trigger card (re-)initialisation.
    fn write(&self, blocks: &[Block], start_block_idx: BlockIdx) -> Result<(), Self::Error> {
        let mut inner = self.inner.borrow_mut();
        crate::debug!("Writing {} blocks @ {}", blocks.len(), start_block_idx.0);
        inner.check_init()?;
        inner.write(blocks, start_block_idx)
    }

    /// Determine how many blocks this device can hold.
    ///
    /// This will trigger card (re-)initialisation.
    fn num_blocks(&self) -> Result<BlockCount, Self::Error> {
        let mut inner = self.inner.borrow_mut();
        inner.check_init()?;
        inner.num_blocks()
    }
}

/// Inner details for the SD Card driver.
///
/// All the APIs required `&mut self`.
struct SdCardInner<SPI, DELAYER>
where
    SPI: embedded_hal::spi::SpiDevice<u8>,
    DELAYER: embedded_hal::delay::DelayNs,
{
    spi: SPI,
    delayer: DELAYER,
    card_type: Option<CardType>,
    options: AcquireOpts,
}

impl<SPI, DELAYER> SdCardInner<SPI, DELAYER>
where
    SPI: embedded_hal::spi::SpiDevice<u8>,
    DELAYER: embedded_hal::delay::DelayNs,
{
    /// Read one or more blocks, starting at the given block index.
    fn read(&mut self, blocks: &mut [Block], start_block_idx: BlockIdx) -> Result<(), Error> {
        let start_idx = match self.card_type {
            Some(CardType::SD1 | CardType::SD2) => start_block_idx.0 * 512,
            Some(CardType::SdhcSdxc) => start_block_idx.0,
            None => return Err(Error::CardNotFound),
        };

        if blocks.len() == 1 {
            // Start a single-block read
            self.card_command(CmdId::CMD17_ReadSingleBlock, start_idx)?;
            self.read_data(&mut blocks[0].contents)?;
        } else {
            // Start a multi-block read. A card that never became ready was
            // sent nothing, and that failure returns as it is.
            self.await_ready_for(CmdId::CMD18_ReadMultipleBlock)?;
            // From the write on, the card may be streaming: once it takes
            // CMD18 it sends blocks until told to stop, and neither a write
            // that reports an error (the transaction can fail flushing or
            // releasing chip select after the bytes went out) nor a response
            // that did not arrive says whether it took it. So every way out
            // sends CMD12, or the next command would talk to a card still
            // sending the last one. The first error is the one reported.
            let mut data = self
                .write_command(CmdId::CMD18_ReadMultipleBlock, start_idx)
                .and_then(|()| self.command_response(CmdId::CMD18_ReadMultipleBlock))
                .map(|_| ());
            if data.is_ok() {
                for block in blocks.iter_mut() {
                    data = self.read_data(&mut block.contents);
                    if data.is_err() {
                        break;
                    }
                }
            }
            let stopped = self.card_command(CmdId::CMD12_StopTransmission, 0);
            data?;
            stopped?;
        }
        Ok(())
    }

    /// Write one or more blocks, starting at the given block index.
    fn write(&mut self, blocks: &[Block], start_block_idx: BlockIdx) -> Result<(), Error> {
        let start_idx = match self.card_type {
            Some(CardType::SD1 | CardType::SD2) => start_block_idx.0 * 512,
            Some(CardType::SdhcSdxc) => start_block_idx.0,
            None => return Err(Error::CardNotFound),
        };
        if blocks.len() == 1 {
            // Start a single-block write
            self.card_command(CmdId::CMD24_WriteBlock, start_idx)?;
            self.write_data(DATA_START_BLOCK, &blocks[0].contents)?;
            self.wait_not_busy(Delay::new_write())?;
            if self.card_command(CmdId::CMD13_SendStatus, 0)? != 0x00 {
                return Err(Error::WriteError);
            }
            if self.read_byte()? != 0x00 {
                return Err(Error::WriteError);
            }
        } else {
            // > It is recommended using this command preceding CMD25, some of the cards will be faster for Multiple
            // > Write Blocks operation. Note that the host should send ACMD23 just before WRITE command if the host
            // > wants to use the pre-erased feature
            self.card_acmd(AcmdId::ACMD23_PreErase, blocks.len() as u32)?;
            // wait for card to be ready before sending the next command
            self.wait_not_busy(Delay::new_write())?;

            // Start a multi-block write. As for the multi-block read, a card
            // that never became ready was sent nothing; from the attempt to
            // write CMD25 on, the card may be taking blocks, and only the
            // stop token ends that. So every way out sends it, and the first
            // error is the one reported.
            self.await_ready_for(CmdId::CMD25_WriteMultipleBlock)?;
            let mut data = self
                .write_command(CmdId::CMD25_WriteMultipleBlock, start_idx)
                .and_then(|()| self.command_response(CmdId::CMD25_WriteMultipleBlock))
                .map(|_| ());
            if data.is_ok() {
                for block in blocks.iter() {
                    data = self
                        .wait_not_busy(Delay::new_write())
                        .and_then(|()| self.write_data(WRITE_MULTIPLE_TOKEN, &block.contents));
                    if data.is_err() {
                        break;
                    }
                }
            }
            // Stop the write
            let stopped = self
                .wait_not_busy(Delay::new_write())
                .and_then(|()| self.write_byte(STOP_TRAN_TOKEN));
            data?;
            stopped?;
        }
        Ok(())
    }

    /// Determine how many blocks this device can hold.
    fn num_blocks(&mut self) -> Result<BlockCount, Error> {
        let csd = self.read_csd()?;
        crate::debug!("CSD: {:?}", csd);
        Ok(BlockCount(csd.card_capacity_blocks()))
    }

    /// Return the usable size of this SD card in bytes.
    fn num_bytes(&mut self) -> Result<u64, Error> {
        let csd = self.read_csd()?;
        crate::debug!("CSD: {:?}", csd);
        Ok(csd.card_capacity_bytes())
    }

    /// Can this card erase single blocks?
    pub fn erase_single_block_enabled(&mut self) -> Result<bool, Error> {
        let csd = self.read_csd()?;
        Ok(csd.erase_single_block_enabled())
    }

    /// Read the 'card specific data' block.
    fn read_csd(&mut self) -> Result<csd::Csd, Error> {
        if self.card_type.is_none() {
            return Err(Error::CardNotFound);
        }

        if self.card_command(CmdId::CMD9_SendCsd, 0)? != 0 {
            return Err(Error::RegisterReadError);
        }
        let mut csd_raw: [u8; 16] = [0; 16];
        self.read_data(&mut csd_raw)?;

        // Select the CSD layout from the CSD_STRUCTURE field (bits 127:126);
        // note that it is independent from the Physical Layer v2.00+ (`card_type`).
        csd::Csd::new(&csd_raw).map_err(|_| Error::RegisterReadError)
    }

    /// Read an arbitrary number of bytes from the card using the SD Card
    /// protocol and an optional CRC. Always fills the given buffer, so make
    /// sure it's the right size.
    fn read_data(&mut self, buffer: &mut [u8]) -> Result<(), Error> {
        // Get first non-FF byte.
        let mut delay = Delay::new_read();
        let status = loop {
            let s = self.read_byte()?;
            if s != 0xFF {
                break s;
            }
            delay.delay(&mut self.delayer, Error::TimeoutReadBuffer)?;
        };
        if status != DATA_START_BLOCK {
            return Err(Error::ReadError);
        }

        buffer.fill(0xFF);
        self.transfer_bytes(buffer)?;

        // These two bytes are always sent. They are either a valid CRC, or
        // junk, depending on whether CRC mode was enabled.
        let mut crc_bytes = [0xFF; 2];
        self.transfer_bytes(&mut crc_bytes)?;
        if self.options.use_crc {
            let crc = u16::from_be_bytes(crc_bytes);
            let calc_crc = crc16(buffer);
            if crc != calc_crc {
                return Err(Error::CrcError(crc, calc_crc));
            }
        }

        Ok(())
    }

    /// Write an arbitrary number of bytes to the card using the SD protocol and
    /// an optional CRC.
    fn write_data(&mut self, token: u8, buffer: &[u8]) -> Result<(), Error> {
        self.write_byte(token)?;
        self.write_bytes(buffer)?;
        let crc_bytes = if self.options.use_crc {
            crc16(buffer).to_be_bytes()
        } else {
            [0xFF, 0xFF]
        };
        // These two bytes are always sent. They are either a valid CRC, or
        // junk, depending on whether CRC mode was enabled.
        self.write_bytes(&crc_bytes)?;

        let status = self.read_byte()?;
        if (status & DATA_RES_MASK) != DATA_RES_ACCEPTED {
            Err(Error::WriteError)
        } else {
            Ok(())
        }
    }

    /// Check the card is initialised.
    fn check_init(&mut self) -> Result<(), Error> {
        if self.card_type.is_none() {
            // If we don't know what the card type is, try and initialise the
            // card. This will tell us what type of card it is.
            self.acquire()
        } else {
            Ok(())
        }
    }

    /// Initializes the card into a known state (or at least tries to).
    fn acquire(&mut self) -> Result<(), Error> {
        crate::debug!("acquiring card with opts: {:?}", self.options);
        let f = |s: &mut Self| {
            // Assume it hasn't worked
            let mut card_type;
            crate::trace!("Reset card..");
            // Enter SPI mode.
            let mut delay = Delay::new(s.options.acquire_retries);
            for _attempts in 1.. {
                crate::trace!("Enter SPI mode, attempt: {}..", _attempts);
                match s.card_command(CmdId::CMD0_GoIdleState, 0) {
                    Err(Error::TimeoutCommand(CmdId::CMD0_GoIdleState)) => {
                        // Try again?
                        crate::warn!("Timed out, trying again..");
                        // Try flushing the card as done here: https://github.com/greiman/SdFat/blob/master/src/SdCard/SdSpiCard.cpp#L170,
                        // https://github.com/rust-embedded-community/embedded-sdmmc-rs/pull/65#issuecomment-1270709448
                        for _ in 0..0xFF {
                            s.write_byte(0xFF)?;
                        }
                    }
                    Err(e) => {
                        return Err(e);
                    }
                    Ok(R1_IDLE_STATE) => {
                        break;
                    }
                    Ok(_r) => {
                        // Try again
                        crate::trace!("Got response: {:x}, trying again..", _r);
                    }
                }

                delay.delay(&mut s.delayer, Error::CardNotFound)?;
            }
            // Enable CRC
            crate::debug!("Enable CRC: {}", s.options.use_crc);
            // "The SPI interface is initialized in the CRC OFF mode in default"
            // -- SD Part 1 Physical Layer Specification v9.00, Section 7.2.2 Bus Transfer Protection
            if s.options.use_crc && s.card_command(CmdId::CMD59_CrcOnOff, 1)? != R1_IDLE_STATE {
                return Err(Error::CantEnableCRC);
            }
            // Check card version
            let mut delay = Delay::new_command();
            let arg = loop {
                if s.card_command(CmdId::CMD8_SendIfCond, 0x1AA)?
                    == (R1_ILLEGAL_COMMAND | R1_IDLE_STATE)
                {
                    card_type = CardType::SD1;
                    break 0;
                }
                let mut buffer = [0xFF; 4];
                s.transfer_bytes(&mut buffer)?;
                let status = buffer[3];
                if status == 0xAA {
                    card_type = CardType::SD2;
                    break 0x4000_0000;
                }
                delay.delay(
                    &mut s.delayer,
                    Error::TimeoutCommand(CmdId::CMD8_SendIfCond),
                )?;
            };

            let mut delay = Delay::new_command();
            while s.card_acmd(AcmdId::ACMD41_SdSendOpCond, arg)? != R1_READY_STATE {
                delay.delay(
                    &mut s.delayer,
                    Error::TimeoutACommand(AcmdId::ACMD41_SdSendOpCond),
                )?;
            }

            if card_type == CardType::SD2 {
                if s.card_command(CmdId::CMD58_ReadOcr, 0)? != 0 {
                    return Err(Error::Cmd58Error);
                }
                let mut buffer = [0xFF; 4];
                s.transfer_bytes(&mut buffer)?;
                if (buffer[0] & 0xC0) == 0xC0 {
                    card_type = CardType::SdhcSdxc;
                }
                // Ignore the other three bytes
            }
            crate::debug!("Card version: {:?}", card_type);
            s.card_type = Some(card_type);
            Ok(())
        };
        let result = f(self);
        let _ = self.read_byte();
        result
    }

    /// Perform an application-specific command.
    fn card_acmd(&mut self, command: AcmdId, arg: u32) -> Result<u8, Error> {
        self.card_command(CmdId::CMD55_AppCmd, 0)?;
        self.card_acmd_after_escape(command, arg)
    }

    fn card_acmd_after_escape(&mut self, command: AcmdId, arg: u32) -> Result<u8, Error> {
        // Wait for the required idle gap (Ncc) after the CMD55 escape response
        // before clocking out the application command.
        self.wait_not_busy(Delay::new_command())?;
        let mut buf = [
            0x40 | command as u8,
            (arg >> 24) as u8,
            (arg >> 16) as u8,
            (arg >> 8) as u8,
            arg as u8,
            0,
        ];
        buf[5] = (crc7(&buf[0..5]) << 1) | 1;

        self.write_bytes(&buf)?;

        let mut delay = Delay::new_command();
        loop {
            let result = self.read_byte()?;
            if (result & 0x80) == ERROR_OK {
                return Ok(result);
            }
            delay.delay(&mut self.delayer, Error::TimeoutACommand(command))?;
        }
    }

    /// Perform a command.
    fn card_command(&mut self, command: CmdId, arg: u32) -> Result<u8, Error> {
        self.await_ready_for(command)?;
        self.write_command(command, arg)?;
        self.command_response(command)
    }

    /// Wait until the card will take a command. An error here means nothing
    /// was sent.
    fn await_ready_for(&mut self, command: CmdId) -> Result<(), Error> {
        if command != CmdId::CMD0_GoIdleState && command != CmdId::CMD12_StopTransmission {
            self.wait_not_busy(Delay::new_command())?;
        }
        Ok(())
    }

    /// Put a command frame on the wire. An error here does not say the card
    /// missed it: the SPI transaction can fail after its bytes went out.
    fn write_command(&mut self, command: CmdId, arg: u32) -> Result<(), Error> {
        let mut buf = [
            0x40 | command as u8,
            (arg >> 24) as u8,
            (arg >> 16) as u8,
            (arg >> 8) as u8,
            arg as u8,
            0,
        ];
        buf[5] = (crc7(&buf[0..5]) << 1) | 1;

        self.write_bytes(&buf)
    }

    /// Wait for the R1 response to a command just sent. An error here leaves
    /// open whether the card acted on it.
    fn command_response(&mut self, command: CmdId) -> Result<u8, Error> {
        // skip stuff byte for stop read
        if command == CmdId::CMD12_StopTransmission {
            let _result = self.read_byte()?;
        }

        let mut delay = Delay::new_command();
        loop {
            let result = self.read_byte()?;
            if (result & 0x80) == ERROR_OK {
                return Ok(result);
            }
            delay.delay(&mut self.delayer, Error::TimeoutCommand(command))?;
        }
    }

    /// Receive a byte from the SPI bus by clocking out an 0xFF byte.
    fn read_byte(&mut self) -> Result<u8, Error> {
        self.transfer_byte(0xFF)
    }

    /// Send a byte over the SPI bus and ignore what comes back.
    fn write_byte(&mut self, out: u8) -> Result<(), Error> {
        let _ = self.transfer_byte(out)?;
        Ok(())
    }

    /// Send one byte and receive one byte over the SPI bus.
    fn transfer_byte(&mut self, out: u8) -> Result<u8, Error> {
        let mut read_buf = [0u8; 1];
        self.spi
            .transfer(&mut read_buf, &[out])
            .map_err(|_| Error::Transport)?;
        Ok(read_buf[0])
    }

    /// Send multiple bytes and ignore what comes back over the SPI bus.
    fn write_bytes(&mut self, out: &[u8]) -> Result<(), Error> {
        self.spi.write(out).map_err(|_e| Error::Transport)?;
        Ok(())
    }

    /// Send multiple bytes and replace them with what comes back over the SPI bus.
    fn transfer_bytes(&mut self, in_out: &mut [u8]) -> Result<(), Error> {
        self.spi
            .transfer_in_place(in_out)
            .map_err(|_e| Error::Transport)?;
        Ok(())
    }

    /// Spin until the card returns 0xFF, or we spin too many times and
    /// timeout.
    fn wait_not_busy(&mut self, mut delay: Delay) -> Result<(), Error> {
        loop {
            let s = self.read_byte()?;
            if s == 0xFF {
                break;
            }
            delay.delay(&mut self.delayer, Error::TimeoutWaitNotBusy)?;
        }
        Ok(())
    }
}

/// Options for acquiring the card.
#[cfg_attr(feature = "defmt-log", derive(defmt::Format))]
#[derive(Debug)]
pub struct AcquireOpts {
    /// Set to true to enable CRC checking on reading/writing blocks of data.
    ///
    /// Set to false to disable the CRC. Some cards don't support CRC correctly
    /// and this option may be useful in that instance.
    ///
    /// On by default because without it you might get silent data corruption on
    /// your card.
    pub use_crc: bool,

    /// Sets the number of times we will retry to acquire the card before giving up and returning
    /// `Err(Error::CardNotFound)`. By default, card acquisition will be retried 50 times.
    pub acquire_retries: u32,
}

impl Default for AcquireOpts {
    fn default() -> Self {
        AcquireOpts {
            use_crc: true,
            acquire_retries: 50,
        }
    }
}

/// The possible errors this crate can generate.
#[cfg_attr(feature = "defmt-log", derive(defmt::Format))]
#[derive(Debug, Copy, Clone)]
pub enum Error {
    /// We got an error from the SPI peripheral
    Transport,
    /// We failed to enable CRC checking on the SD card
    CantEnableCRC,
    /// We didn't get a response when reading data from the card
    TimeoutReadBuffer,
    /// We didn't get a response when waiting for the card to not be busy
    TimeoutWaitNotBusy,
    /// We didn't get a response when executing this command
    TimeoutCommand(CmdId),
    /// We didn't get a response when executing this application-specific command
    TimeoutACommand(AcmdId),
    /// We got a bad response from Command 58
    Cmd58Error,
    /// We failed to read the Card Specific Data register
    RegisterReadError,
    /// We got a CRC mismatch (card gave us, we calculated)
    CrcError(u16, u16),
    /// Error reading from the card
    ReadError,
    /// Error writing to the card
    WriteError,
    /// Can't perform this operation with the card in this state
    BadState,
    /// Couldn't find the card
    CardNotFound,
    /// Couldn't set a GPIO pin
    GpioError,
}

impl core::fmt::Display for Error {
    fn fmt(&self, f: &mut core::fmt::Formatter) -> core::fmt::Result {
        match self {
            Error::Transport => write!(f, "error from SPI peripheral"),
            Error::CantEnableCRC => write!(f, "failed to enable CRC checking"),
            Error::TimeoutReadBuffer => write!(f, "timeout when reading data"),
            Error::TimeoutWaitNotBusy => write!(f, "timeout when waiting for card to not be busy"),
            Error::TimeoutCommand(command) => {
                write!(f, "timeout when executing command {command:?}")
            }
            Error::TimeoutACommand(command) => write!(
                f,
                "timeout when executing application-specific command {command:?}"
            ),
            Error::Cmd58Error => write!(f, "bad response from command 58"),
            Error::RegisterReadError => write!(f, "failed to read Card Specific Data register"),
            Error::CrcError(_, _) => write!(f, "CRC mismatch"),
            Error::ReadError => write!(f, "read error"),
            Error::WriteError => write!(f, "write error"),
            Error::BadState => write!(f, "cannot perform operation with card in thiis state"),
            Error::CardNotFound => write!(f, "card not found"),
            Error::GpioError => write!(f, "cannot set GPIO pin"),
        }
    }
}

impl core::error::Error for Error {}

/// This an object you can use to busy-wait with a timeout.
///
/// Will let you call `delay` up to `max_retries` times before `delay` returns
/// an error.
struct Delay {
    retries_left: u32,
}

impl Delay {
    /// The default number of retries for a read operation.
    ///
    /// At ~10us each this is ~100ms.
    ///
    /// See `Part1_Physical_Layer_Simplified_Specification_Ver9.00-1.pdf` Section 4.6.2.1
    pub const DEFAULT_READ_RETRIES: u32 = 10_000;

    /// The default number of retries for a write operation.
    ///
    /// At ~10us each this is ~500ms.
    ///
    /// See `Part1_Physical_Layer_Simplified_Specification_Ver9.00-1.pdf` Section 4.6.2.2
    pub const DEFAULT_WRITE_RETRIES: u32 = 50_000;

    /// The default number of retries for a control command.
    ///
    /// At ~10us each this is ~100ms.
    ///
    /// No value is given in the specification, so we pick the same as the read timeout.
    pub const DEFAULT_COMMAND_RETRIES: u32 = 10_000;

    /// Create a new Delay object with the given maximum number of retries.
    fn new(max_retries: u32) -> Delay {
        Delay {
            retries_left: max_retries,
        }
    }

    /// Create a new Delay object with the maximum number of retries for a read operation.
    fn new_read() -> Delay {
        Delay::new(Self::DEFAULT_READ_RETRIES)
    }

    /// Create a new Delay object with the maximum number of retries for a write operation.
    fn new_write() -> Delay {
        Delay::new(Self::DEFAULT_WRITE_RETRIES)
    }

    /// Create a new Delay object with the maximum number of retries for a command operation.
    fn new_command() -> Delay {
        Delay::new(Self::DEFAULT_COMMAND_RETRIES)
    }

    /// Wait for a while.
    ///
    /// Checks the retry counter first, and if we hit the max retry limit, the
    /// value `err` is returned. Otherwise we wait for 10us and then return
    /// `Ok(())`.
    fn delay<T>(&mut self, delayer: &mut T, err: Error) -> Result<(), Error>
    where
        T: embedded_hal::delay::DelayNs,
    {
        if self.retries_left == 0 {
            Err(err)
        } else {
            delayer.delay_us(10);
            self.retries_left -= 1;
            Ok(())
        }
    }
}

// ****************************************************************************
//
// End Of File
//
// ****************************************************************************

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;

    /// Enough of an SD card on SPI to answer a single- or multi-block read.
    /// After CMD18 it streams blocks until CMD12, as a card does, so bytes a
    /// driver leaves unread are still waiting for whatever it sends next.
    struct ScriptedCard {
        outgoing: VecDeque<u8>,
        commands: Vec<u8>,
        /// The block of a multi-block read that arrives with a bad token.
        fail_block: Option<usize>,
        /// Fail the first read after a CMD18 frame, losing its response
        /// while the card, having taken the command, streams on.
        lose_cmd18_response: bool,
        glitch_armed: bool,
        /// Hold the data line low, as a card still busy with a write does.
        busy: bool,
        /// Report the transaction carrying this command's frame as failed
        /// after the card took the frame, as a failed chip-select release
        /// would.
        fail_command_write: Option<u8>,
        /// Taking blocks after CMD25, until the stop token.
        writing: bool,
        blocks_taken: usize,
        /// The block of a multi-block write the card answers as rejected.
        reject_block: Option<usize>,
        /// Blocks the card accepted.
        written: Vec<[u8; 512]>,
    }

    /// A transfer the bus reported as failed.
    #[derive(Debug)]
    struct Glitch;

    impl embedded_hal::spi::Error for Glitch {
        fn kind(&self) -> embedded_hal::spi::ErrorKind {
            embedded_hal::spi::ErrorKind::Other
        }
    }

    impl ScriptedCard {
        fn block(n: u32) -> [u8; 512] {
            core::array::from_fn(|i| (n as usize * 7 + i) as u8)
        }

        fn queue_block(&mut self, n: u32) {
            self.outgoing.push_back(DATA_START_BLOCK);
            self.outgoing.extend(Self::block(n));
            self.outgoing.extend([0, 0]);
        }

        fn command(&mut self, frame: &[u8]) {
            let command = frame[0] & 0x3F;
            let arg = u32::from_be_bytes([frame[1], frame[2], frame[3], frame[4]]);
            self.commands.push(command);
            match command {
                // CMD17: one block.
                17 => {
                    self.outgoing.push_back(0x00);
                    self.queue_block(arg);
                }
                // CMD18: blocks from `arg` on, more than any test reads.
                18 => {
                    self.glitch_armed = self.lose_cmd18_response;
                    self.outgoing.push_back(0x00);
                    for i in 0..8 {
                        if Some(i) == self.fail_block {
                            // A data error token instead of a block.
                            self.outgoing.push_back(0x0B);
                        } else {
                            self.queue_block(arg + i as u32);
                        }
                    }
                }
                // CMD25: blocks are taken from here until the stop token.
                25 => {
                    self.outgoing.push_back(0x00);
                    self.writing = true;
                    self.blocks_taken = 0;
                }
                // CMD12: the stream stops; a stuff byte, then R1.
                12 => {
                    self.outgoing.clear();
                    self.outgoing.extend([0xFF, 0x00]);
                }
                _ => self.outgoing.push_back(0x04),
            }
        }

        /// Bytes the driver clocks out one at a time: the tokens of a write.
        fn sent(&mut self, bytes: &[u8]) {
            if !self.writing || bytes.len() != 1 {
                return;
            }
            match bytes[0] {
                // A block follows; its data response comes after the CRC.
                WRITE_MULTIPLE_TOKEN => {
                    let response = if Some(self.blocks_taken) == self.reject_block {
                        0x0B
                    } else {
                        DATA_RES_ACCEPTED
                    };
                    self.outgoing.push_back(response);
                    self.blocks_taken += 1;
                }
                STOP_TRAN_TOKEN => {
                    self.writing = false;
                    self.commands.push(STOP_TRAN_TOKEN);
                }
                _ => {}
            }
        }

        fn next(&mut self) -> u8 {
            if self.busy {
                return 0x00;
            }
            self.outgoing.pop_front().unwrap_or(0xFF)
        }

        /// Clock `bytes` in from the card, unless a glitch is armed: then the
        /// first byte is lost on the wire and the transfer fails.
        fn receive(&mut self, bytes: &mut [u8]) -> Result<(), Glitch> {
            if self.glitch_armed {
                self.glitch_armed = false;
                let _ = self.next();
                return Err(Glitch);
            }
            bytes.iter_mut().for_each(|b| *b = self.next());
            Ok(())
        }
    }

    impl embedded_hal::spi::ErrorType for ScriptedCard {
        type Error = Glitch;
    }

    impl embedded_hal::spi::SpiDevice<u8> for ScriptedCard {
        fn transaction(
            &mut self,
            operations: &mut [embedded_hal::spi::Operation<'_, u8>],
        ) -> Result<(), Self::Error> {
            use embedded_hal::spi::Operation;
            for operation in operations {
                match operation {
                    Operation::Write(bytes) => {
                        if bytes.len() == 6 && bytes[0] & 0xC0 == 0x40 {
                            self.command(bytes);
                            if Some(bytes[0] & 0x3F) == self.fail_command_write {
                                return Err(Glitch);
                            }
                        } else if bytes.len() == 512 && self.writing {
                            let mut block = [0u8; 512];
                            block.copy_from_slice(bytes);
                            if Some(self.blocks_taken) != self.reject_block {
                                self.written.push(block);
                            }
                        }
                    }
                    Operation::Transfer(read, write) => {
                        self.receive(read)?;
                        self.sent(write);
                    }
                    Operation::TransferInPlace(bytes) => self.receive(bytes)?,
                    Operation::Read(bytes) => self.receive(bytes)?,
                    Operation::DelayNs(_) => {}
                }
            }
            Ok(())
        }
    }

    struct NoDelay;

    impl embedded_hal::delay::DelayNs for NoDelay {
        fn delay_ns(&mut self, _ns: u32) {}
    }

    fn card(fail_block: Option<usize>) -> SdCardInner<ScriptedCard, NoDelay> {
        SdCardInner {
            spi: ScriptedCard {
                outgoing: VecDeque::new(),
                commands: Vec::new(),
                fail_block,
                lose_cmd18_response: false,
                glitch_armed: false,
                busy: false,
                fail_command_write: None,
                writing: false,
                blocks_taken: 0,
                reject_block: None,
                written: Vec::new(),
            },
            delayer: NoDelay,
            card_type: Some(CardType::SdhcSdxc),
            options: AcquireOpts {
                use_crc: false,
                acquire_retries: 1,
            },
        }
    }

    #[test]
    fn a_multi_block_read_reads_every_block_and_stops_the_card() {
        let mut card = card(None);
        let mut blocks = [Block::new(), Block::new(), Block::new()];
        card.read(&mut blocks, BlockIdx(40)).expect("read");
        for (i, block) in blocks.iter().enumerate() {
            assert_eq!(block.contents, ScriptedCard::block(40 + i as u32));
        }
        assert_eq!(card.spi.commands, [18, 12]);
    }

    /// A block that fails part way still stops the card, the data's error is
    /// the one reported, and the next command finds a card that is listening.
    #[test]
    fn a_multi_block_read_that_fails_part_way_still_stops_the_card() {
        let mut card = card(Some(1));
        let mut blocks = [Block::new(), Block::new(), Block::new()];
        assert!(matches!(
            card.read(&mut blocks, BlockIdx(40)),
            Err(Error::ReadError)
        ));
        assert_eq!(card.spi.commands, [18, 12], "CMD12 follows the failure");

        let mut again = [Block::new()];
        card.read(&mut again, BlockIdx(90))
            .expect("the next read works");
        assert_eq!(again[0].contents, ScriptedCard::block(90));
    }

    /// The CMD18 frame goes out and its response is lost on the wire. The
    /// card took the command and is streaming, so CMD12 still follows, the
    /// response's error is the one reported, and the next read works.
    #[test]
    fn a_multi_block_read_whose_response_is_lost_still_stops_the_card() {
        let mut card = card(None);
        card.spi.lose_cmd18_response = true;
        let mut blocks = [Block::new(), Block::new()];
        assert!(matches!(
            card.read(&mut blocks, BlockIdx(40)),
            Err(Error::Transport)
        ));
        assert_eq!(
            card.spi.commands,
            [18, 12],
            "CMD12 follows the lost response"
        );

        let mut again = [Block::new()];
        card.read(&mut again, BlockIdx(90))
            .expect("the next read works");
        assert_eq!(again[0].contents, ScriptedCard::block(90));
    }

    /// A card that stays busy is never sent CMD18, so there is no stream to
    /// stop and no CMD12 goes after it.
    #[test]
    fn a_multi_block_read_the_card_was_too_busy_for_sends_nothing() {
        let mut card = card(None);
        card.spi.busy = true;
        let mut blocks = [Block::new(), Block::new()];
        assert!(matches!(
            card.read(&mut blocks, BlockIdx(40)),
            Err(Error::TimeoutWaitNotBusy)
        ));
        assert!(card.spi.commands.is_empty(), "{:?}", card.spi.commands);
    }

    /// The CMD18 frame reaches the card, which starts streaming, and then the
    /// write's transaction reports a failure. The write's error says nothing
    /// about whether the card took the command, so CMD12 still follows, the
    /// write's error is the one reported, and the next read works.
    #[test]
    fn a_multi_block_read_whose_command_write_reports_failure_still_stops_the_card() {
        let mut card = card(None);
        card.spi.fail_command_write = Some(18);
        let mut blocks = [Block::new(), Block::new()];
        assert!(matches!(
            card.read(&mut blocks, BlockIdx(40)),
            Err(Error::Transport)
        ));
        assert_eq!(
            card.spi.commands,
            [18, 12],
            "CMD12 follows the failed write"
        );

        card.spi.fail_command_write = None;
        let mut again = [Block::new()];
        card.read(&mut again, BlockIdx(90))
            .expect("the next read works");
        assert_eq!(again[0].contents, ScriptedCard::block(90));
    }

    fn blocks(n: u32) -> Vec<Block> {
        (0..n)
            .map(|i| {
                let mut block = Block::new();
                block.contents = ScriptedCard::block(100 + i);
                block
            })
            .collect()
    }

    #[test]
    fn a_multi_block_write_sends_every_block_and_the_stop_token() {
        let mut card = card(None);
        card.write(&blocks(3), BlockIdx(40)).expect("write");
        assert_eq!(card.spi.written.len(), 3);
        assert_eq!(card.spi.commands, [55, 23, 25, STOP_TRAN_TOKEN]);
    }

    /// A block the card rejects part way still ends with the stop token, the
    /// rejection is the error reported, and the next command finds a card
    /// that is listening.
    #[test]
    fn a_multi_block_write_that_fails_part_way_still_sends_the_stop_token() {
        let mut card = card(None);
        card.spi.reject_block = Some(1);
        assert!(matches!(
            card.write(&blocks(3), BlockIdx(40)),
            Err(Error::WriteError)
        ));
        assert_eq!(card.spi.commands, [55, 23, 25, STOP_TRAN_TOKEN]);
        assert_eq!(card.spi.written.len(), 1, "the card took one block");

        let mut again = [Block::new()];
        card.read(&mut again, BlockIdx(90))
            .expect("the next read works");
        assert_eq!(again[0].contents, ScriptedCard::block(90));
    }

    /// The CMD25 frame reaches the card and the write reports failure: the
    /// card may be taking blocks, so the stop token still follows.
    #[test]
    fn a_multi_block_write_whose_command_write_reports_failure_still_sends_the_stop_token() {
        let mut card = card(None);
        card.spi.fail_command_write = Some(25);
        assert!(matches!(
            card.write(&blocks(2), BlockIdx(40)),
            Err(Error::Transport)
        ));
        assert_eq!(card.spi.commands, [55, 23, 25, STOP_TRAN_TOKEN]);
    }
}
