use crate::sources::{BufferStatus, Source, SourceError};
use crate::TraceData;

use std::fs;
use std::io::Read;
use std::os::unix::io::{AsRawFd, RawFd};
use std::time::Duration;

use itm_decode::{Decoder, DecoderOptions};
use nix::{
    fcntl::{self, FcntlArg, OFlag},
    libc,
    sys::termios::{
        self, BaudRate, ControlFlags, InputFlags, LocalFlags, OutputFlags, SetArg,
        SpecialCharacterIndices as CC,
    },
    unistd::{sysconf, SysconfVar},
};
use probe_rs::Session;

mod ioctl {
    use super::libc;
    use nix::{ioctl_none_bad, ioctl_read_bad, ioctl_write_int_bad, ioctl_write_ptr_bad};

    ioctl_none_bad!(tiocexcl, libc::TIOCEXCL);
    ioctl_read_bad!(tiocmget, libc::TIOCMGET, libc::c_int);
    ioctl_read_bad!(fionread, libc::FIONREAD, libc::c_int);
    ioctl_write_ptr_bad!(tiocmset, libc::TIOCMSET, libc::c_int);
    ioctl_write_int_bad!(tcflsh, libc::TCFLSH);
}

/// Opens and configures the given `device`.
///
/// Effectively mirrors the behavior of
/// ```
/// $ screen /dev/ttyUSB3 115200
/// ```
/// assuming that `device` is `/dev/ttyUSB3`.
///
/// TODO ensure POSIX compliance, see termios(3)
/// TODO We are currently using line disciple 0. Is that correct?
pub fn configure(device: &String) -> Result<fs::File, SourceError> {
    let file = fs::OpenOptions::new()
        .read(true)
        .open(&device)
        .map_err(SourceError::SetupIOError)?;

    unsafe {
        let fd = file.as_raw_fd();

        // Enable exclusive mode. Any further open(2) will fail with EBUSY.
        ioctl::tiocexcl(fd).map_err(|e| {
            SourceError::SetupError(format!(
                "Failed to put {} into exclusive mode: tiocexcl = {}",
                device, e
            ))
        })?;

        let mut settings = termios::tcgetattr(fd).map_err(|e| {
            SourceError::SetupError(format!(
                "Failed to read terminal settings of {}: tcgetattr = {}",
                device, e
            ))
        })?;

        settings.input_flags |= InputFlags::BRKINT | InputFlags::IGNPAR | InputFlags::IXON;
        settings.input_flags &= !(InputFlags::ICRNL
            | InputFlags::IGNBRK
            | InputFlags::PARMRK
            | InputFlags::INPCK
            | InputFlags::ISTRIP
            | InputFlags::INLCR
            | InputFlags::IGNCR
            | InputFlags::ICRNL
            | InputFlags::IXOFF
            | InputFlags::IXANY
            | InputFlags::IMAXBEL
            | InputFlags::IUTF8);

        settings.output_flags |= OutputFlags::NL0
            | OutputFlags::CR0
            | OutputFlags::TAB0
            | OutputFlags::BS0
            | OutputFlags::VT0
            | OutputFlags::FF0;
        settings.output_flags &= !(OutputFlags::OPOST
            | OutputFlags::ONLCR
            | OutputFlags::OLCUC
            | OutputFlags::OCRNL
            | OutputFlags::ONOCR
            | OutputFlags::ONLRET
            | OutputFlags::OFILL
            | OutputFlags::OFDEL
            | OutputFlags::NL1
            | OutputFlags::CR1
            | OutputFlags::CR2
            | OutputFlags::CR3
            | OutputFlags::TAB1
            | OutputFlags::TAB2
            | OutputFlags::TAB3
            | OutputFlags::XTABS
            | OutputFlags::BS1
            | OutputFlags::VT1
            | OutputFlags::FF1
            | OutputFlags::NLDLY
            | OutputFlags::CRDLY
            | OutputFlags::TABDLY
            | OutputFlags::BSDLY
            | OutputFlags::VTDLY
            | OutputFlags::FFDLY);

        settings.control_flags |= ControlFlags::CS6
            | ControlFlags::CS7
            | ControlFlags::CS8
            | ControlFlags::CREAD
            | ControlFlags::CLOCAL
            | ControlFlags::CBAUDEX // NOTE also via cfsetspeed below
            | ControlFlags::CSIZE;
        settings.control_flags &= !(ControlFlags::HUPCL
            | ControlFlags::CS5
            | ControlFlags::CSTOPB
            | ControlFlags::PARENB
            | ControlFlags::PARODD
            | ControlFlags::CRTSCTS
            | ControlFlags::CBAUD // NOTE also set via cfsetspeed below?
            | ControlFlags::CMSPAR
            | ControlFlags::CIBAUD);

        settings.local_flags |= LocalFlags::ECHOKE
            | LocalFlags::ECHOE
            | LocalFlags::ECHOK
            | LocalFlags::ECHOCTL
            | LocalFlags::IEXTEN;
        settings.local_flags &= !(LocalFlags::ECHO
            | LocalFlags::ISIG
            | LocalFlags::ICANON
            | LocalFlags::ECHONL
            | LocalFlags::ECHOPRT
            | LocalFlags::EXTPROC
            | LocalFlags::TOSTOP
            | LocalFlags::FLUSHO
            | LocalFlags::PENDIN
            | LocalFlags::NOFLSH);

        termios::cfsetspeed(&mut settings, BaudRate::B115200).map_err(|e| {
            SourceError::SetupError(format!(
                "Failed to configure {} baud rate: cfsetspeed = {}",
                device, e
            ))
        })?;

        settings.control_chars[CC::VTIME as usize] = 2;
        settings.control_chars[CC::VMIN as usize] = 100;

        // Drain all output, flush all input, and apply settings.
        termios::tcsetattr(fd, SetArg::TCSAFLUSH, &settings).map_err(|e| {
            SourceError::SetupError(format!(
                "Failed to apply terminal settings to {}: tcsetattr = {}",
                device, e
            ))
        })?;

        let mut flags: libc::c_int = 0;
        ioctl::tiocmget(fd, &mut flags).map_err(|e| {
            SourceError::SetupError(format!(
                "Failed to read modem bits of {}: tiocmget = {}",
                device, e
            ))
        })?;
        flags |= libc::TIOCM_DTR | libc::TIOCM_RTS;
        ioctl::tiocmset(fd, &flags).map_err(|e| {
            SourceError::SetupError(format!(
                "Failed to apply modem bits to {}: tiocmset = {}",
                device, e
            ))
        })?;

        // Make the tty read-only.
        fcntl::fcntl(fd, FcntlArg::F_SETFL(OFlag::O_RDONLY)).map_err(|e| {
            SourceError::SetupError(format!(
                "Failed to make {} read-only: fcntl = {}",
                device, e
            ))
        })?;

        // Flush all pending I/O, just in case.
        ioctl::tcflsh(fd, libc::TCIOFLUSH).map_err(|e| {
            SourceError::SetupError(format!("Failed to flush I/O of {}: tcflsh = {}", device, e))
        })?;
    }

    Ok(file)
}

pub struct TTYSource {
    bytes: std::io::Bytes<fs::File>,
    fd: RawFd,
    decoder: Decoder,
    session: Session,
}

impl TTYSource {
    pub fn new(device: fs::File, session: Session) -> Self {
        Self {
            fd: device.as_raw_fd(),
            bytes: device.bytes(),
            decoder: Decoder::new(DecoderOptions::default()),
            session,
        }
    }
}

impl Iterator for TTYSource {
    type Item = Result<TraceData, SourceError>;

    fn next(&mut self) -> Option<Self::Item> {
        for b in &mut self.bytes {
            match b {
                Ok(b) => self.decoder.push(&[b]),
                Err(e) => return Some(Err(SourceError::IterIOError(e))),
            };

            match self.decoder.pull_with_timestamp() {
                None => continue,
                Some(packets) => return Some(Ok(packets)),
            }
        }

        None
    }
}

impl Source for TTYSource {
    fn reset_target(&mut self, reset_halt: bool) -> Result<(), SourceError> {
        let mut core = self.session.core(0).map_err(SourceError::ResetError)?;
        if reset_halt {
            core.reset_and_halt(Duration::from_millis(250))
                .map_err(SourceError::ResetError)?;
        } else {
            core.reset().map_err(SourceError::ResetError)?;
        }

        Ok(())
    }

    fn avail_buffer(&self) -> BufferStatus {
        let avail_bytes = unsafe {
            let mut fionread: libc::c_int = 0;
            if ioctl::fionread(self.fd, &mut fionread).is_err() {
                return BufferStatus::Unknown;
            } else {
                fionread as i64
            }
        };

        if let Ok(Some(page_size)) = sysconf(SysconfVar::PAGE_SIZE) {
            match page_size - avail_bytes {
                n if n < page_size / 4 => BufferStatus::AvailWarn(n, page_size),
                n => BufferStatus::Avail(n),
            }
        } else {
            BufferStatus::Unknown
        }
    }

    fn describe(&self) -> String {
        format!("TTY (fd: {})", self.fd)
    }
}
