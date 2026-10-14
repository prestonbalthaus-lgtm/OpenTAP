//! The startup capability probe and the allowlist it builds (round 3, Part 7
//! and section 8.3): which io_uring opcodes the running kernel supports
//! (`IORING_REGISTER_PROBE`) and which ublk features its driver offers
//! (`UBLK_U_CMD_GET_FEATURES`). The daemon refuses to start when a required
//! capability is missing, and no engine crate issues an opcode or command the
//! allowlist does not contain.
//!
//! The checks are pure and the same on every host; an [`Allowlist`] is built
//! from parts in the simulator and the tests, and by `Allowlist::probe` on
//! Linux.

use std::error::Error;
use std::fmt;
use std::io;

use crate::ublk::{feature_name, UBLK_F_CMD_IOCTL_ENCODE, UBLK_F_USER_COPY, UBLK_F_USER_RECOVERY};
use crate::uring::{
    opcode_name, IORING_OP_FSYNC, IORING_OP_READ, IORING_OP_RECV, IORING_OP_SEND_ZC,
    IORING_OP_URING_CMD, IORING_OP_WRITE, IORING_OP_WRITE_FIXED,
};

/// The io_uring opcodes OpenTAP v0.1 needs: the ublk user copy (`READ`,
/// `WRITE`), stripe writes (`WRITE_FIXED`), the CP flush (`FSYNC`), every
/// ublk command (`URING_CMD`) and replication (`SEND_ZC`, `RECV`).
pub const REQUIRED_OPCODES: [u8; 7] = [
    IORING_OP_READ,
    IORING_OP_WRITE,
    IORING_OP_WRITE_FIXED,
    IORING_OP_FSYNC,
    IORING_OP_URING_CMD,
    IORING_OP_SEND_ZC,
    IORING_OP_RECV,
];

/// The ublk features OpenTAP v0.1 needs: the user copy, recovery after the
/// daemon's death, and the ioctl command encoding.
pub const REQUIRED_UBLK_FEATURES: u64 =
    UBLK_F_USER_COPY | UBLK_F_USER_RECOVERY | UBLK_F_CMD_IOCTL_ENCODE;

/// A set of io_uring opcodes: a 256-bit bitset, one bit per `u8` value.
#[derive(Clone, Copy, Default, PartialEq, Eq, Hash)]
pub struct OpcodeSet {
    bits: [u64; 4],
}

impl OpcodeSet {
    /// The empty set.
    #[must_use]
    pub const fn new() -> Self {
        Self { bits: [0; 4] }
    }

    /// The set of `opcodes`.
    #[must_use]
    pub const fn from_opcodes(opcodes: &[u8]) -> Self {
        let mut set = Self::new();
        let mut i = 0;
        while i < opcodes.len() {
            set = set.with(opcodes[i]);
            i += 1;
        }
        set
    }

    /// The set with `opcode` added.
    #[must_use]
    pub const fn with(mut self, opcode: u8) -> Self {
        self.bits[(opcode >> 6) as usize] |= 1 << (opcode & 63);
        self
    }

    /// The set with `opcode` removed.
    #[must_use]
    pub const fn without(mut self, opcode: u8) -> Self {
        self.bits[(opcode >> 6) as usize] &= !(1 << (opcode & 63));
        self
    }

    /// Adds `opcode`.
    pub fn insert(&mut self, opcode: u8) {
        *self = self.with(opcode);
    }

    /// Whether `opcode` is in the set.
    #[must_use]
    pub const fn contains(&self, opcode: u8) -> bool {
        self.bits[(opcode >> 6) as usize] & (1 << (opcode & 63)) != 0
    }

    /// The number of opcodes in the set.
    #[must_use]
    pub const fn len(&self) -> usize {
        let mut count = 0;
        let mut i = 0;
        while i < self.bits.len() {
            count += self.bits[i].count_ones() as usize;
            i += 1;
        }
        count
    }

    /// Whether the set is empty.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// The opcodes in ascending order.
    pub fn iter(&self) -> impl Iterator<Item = u8> {
        let set = *self;
        (0..=u8::MAX).filter(move |&opcode| set.contains(opcode))
    }
}

impl FromIterator<u8> for OpcodeSet {
    fn from_iter<I: IntoIterator<Item = u8>>(opcodes: I) -> Self {
        let mut set = Self::new();
        for opcode in opcodes {
            set.insert(opcode);
        }
        set
    }
}

impl fmt::Debug for OpcodeSet {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_set().entries(self.iter()).finish()
    }
}

/// What the running kernel can do for OpenTAP: the io_uring opcodes it
/// supports and the ublk features its driver reports.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Allowlist {
    opcodes: OpcodeSet,
    ublk_features: u64,
}

impl Allowlist {
    /// An allowlist of the given opcodes and `UBLK_F_*` bits: what the
    /// simulator and the tests use, and what `Allowlist::probe` builds on Linux.
    #[must_use]
    pub const fn from_parts(opcodes: OpcodeSet, ublk_features: u64) -> Self {
        Self {
            opcodes,
            ublk_features,
        }
    }

    /// The supported opcodes.
    #[must_use]
    pub const fn opcodes(&self) -> OpcodeSet {
        self.opcodes
    }

    /// The reported `UBLK_F_*` bits.
    #[must_use]
    pub const fn ublk_features(&self) -> u64 {
        self.ublk_features
    }

    /// Whether the kernel supports `opcode`.
    #[must_use]
    pub const fn supports_opcode(&self, opcode: u8) -> bool {
        self.opcodes.contains(opcode)
    }

    /// Whether the driver reports every bit of `feature` (`UBLK_F_*`).
    #[must_use]
    pub const fn has_ublk_feature(&self, feature: u64) -> bool {
        self.ublk_features & feature == feature
    }

    /// `Ok` if every opcode in `opcodes` is supported, otherwise the missing
    /// ones, in the order given.
    ///
    /// # Errors
    /// [`MissingCapability`] listing the unsupported opcodes.
    pub fn requires(&self, opcodes: &[u8]) -> Result<(), MissingCapability> {
        self.missing(opcodes, 0).into_result()
    }

    /// `Ok` if the driver reports every bit of `features`, otherwise the
    /// missing bits.
    ///
    /// # Errors
    /// [`MissingCapability`] holding the missing `UBLK_F_*` bits.
    pub fn requires_ublk_features(&self, features: u64) -> Result<(), MissingCapability> {
        self.missing(&[], features).into_result()
    }

    /// Checks [`REQUIRED_OPCODES`] and [`REQUIRED_UBLK_FEATURES`] together:
    /// the startup gate of OpenTAP v0.1.
    ///
    /// # Errors
    /// [`MissingCapability`] listing everything that is absent.
    pub fn check_required(&self) -> Result<(), MissingCapability> {
        self.missing(&REQUIRED_OPCODES, REQUIRED_UBLK_FEATURES)
            .into_result()
    }

    fn missing(&self, opcodes: &[u8], ublk_features: u64) -> MissingCapability {
        MissingCapability {
            opcodes: opcodes
                .iter()
                .copied()
                .filter(|&opcode| !self.supports_opcode(opcode))
                .collect(),
            ublk_features: ublk_features & !self.ublk_features,
        }
    }
}

/// The capabilities a check found absent.
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash)]
pub struct MissingCapability {
    /// Required io_uring opcodes the kernel does not support.
    pub opcodes: Vec<u8>,
    /// Required `UBLK_F_*` bits the driver does not report.
    pub ublk_features: u64,
}

impl MissingCapability {
    /// Whether nothing is missing.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.opcodes.is_empty() && self.ublk_features == 0
    }

    fn into_result(self) -> Result<(), Self> {
        if self.is_empty() {
            Ok(())
        } else {
            Err(self)
        }
    }
}

impl fmt::Display for MissingCapability {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("the kernel lacks")?;
        let mut first = true;
        let mut item = |f: &mut fmt::Formatter<'_>, text: fmt::Arguments<'_>| {
            let sep = if first { " " } else { ", " };
            first = false;
            write!(f, "{sep}{text}")
        };
        for &opcode in &self.opcodes {
            match opcode_name(opcode) {
                Some(name) => item(f, format_args!("{name} ({opcode})"))?,
                None => item(f, format_args!("io_uring opcode {opcode}"))?,
            }
        }
        for bit in 0..u64::BITS {
            let feature = 1u64 << bit;
            if self.ublk_features & feature != 0 {
                match feature_name(feature) {
                    Some(name) => item(f, format_args!("{name}"))?,
                    None => item(f, format_args!("ublk feature bit {bit}"))?,
                }
            }
        }
        if first {
            f.write_str(" nothing")?;
        }
        Ok(())
    }
}

impl Error for MissingCapability {}

impl From<MissingCapability> for io::Error {
    fn from(missing: MissingCapability) -> Self {
        Self::new(io::ErrorKind::Unsupported, missing)
    }
}

#[cfg(target_os = "linux")]
mod linux {
    use std::io;

    use io_uring::{cqueue, squeue, IoUring, Probe};

    use super::{Allowlist, OpcodeSet};
    use crate::ublk::{CtrlCommand, UblkControl};

    /// `user_data` of the `GET_FEATURES` command on the probe's private ring.
    const GET_FEATURES_USER_DATA: u64 = u64::from_be_bytes(*b"otapfeat");

    impl Allowlist {
        /// Probes the running kernel (Linux): the opcodes `ring`'s kernel
        /// supports (`IORING_REGISTER_PROBE`) and the features `control`'s
        /// driver reports (`UBLK_U_CMD_GET_FEATURES`).
        ///
        /// `GET_FEATURES` is a control command, which the driver accepts only
        /// on a ring of 128-byte SQEs, so it goes through a private
        /// two-entry ring rather than `ring`: the reactor rings use 64-byte
        /// SQEs, and the probe must neither flush their submissions nor
        /// consume their completions. The command is submitted and its
        /// completion awaited synchronously, once, at startup.
        ///
        /// # Errors
        /// The `io_uring_register(2)` or `io_uring_setup(2)` error, or the
        /// command's negative result as an OS error (a driver older than
        /// `GET_FEATURES` rejects the command).
        pub fn probe<S, C>(ring: &IoUring<S, C>, control: &UblkControl) -> io::Result<Self>
        where
            S: squeue::EntryMarker,
            C: cqueue::EntryMarker,
        {
            let mut probe = Probe::new();
            ring.submitter().register_probe(&mut probe)?;
            let opcodes: OpcodeSet = (0..=u8::MAX)
                .filter(|&opcode| probe.is_supported(opcode))
                .collect();
            let ublk_features = ublk_features(control)?;
            Ok(Self::from_parts(opcodes, ublk_features))
        }
    }

    /// What became of the `GET_FEATURES` SQE.
    enum Outcome {
        /// The kernel never saw it.
        NotSubmitted(io::Error),
        /// The kernel took it, but its completion was not observed.
        InFlight(io::Error),
        /// Its CQE was reaped, with this result.
        Completed(i32),
    }

    /// Issues `UBLK_U_CMD_GET_FEATURES` on a private ring and returns the
    /// `UBLK_F_*` bits.
    fn ublk_features(control: &UblkControl) -> io::Result<u64> {
        let mut ring = IoUring::<squeue::Entry128, cqueue::Entry>::builder().build(2)?;
        // The driver writes UBLK_FEATURES_LEN bytes here from an io-wq
        // worker. The allocation is owned by this raw pointer, which no Rust
        // reference aliases, until the kernel is known to be done with it.
        let buf: *mut u64 = Box::into_raw(Box::new(0));
        let cmd = CtrlCommand::get_features(buf.expose_provenance() as u64);
        let sqe = control.command_sqe(&cmd).user_data(GET_FEATURES_USER_DATA);
        // SAFETY: `push` requires the entry's parameters to stay valid for
        // the whole operation. The only memory the entry names is `buf`: 8
        // bytes, aligned and writable, from `Box::into_raw` above. It is
        // freed below only once the kernel is done with it (the CQE was
        // reaped, or the SQE never left the ring and the ring is closed
        // first); when that cannot be established (`Outcome::InFlight`) the
        // allocation is leaked instead, so it outlives the kernel's write in
        // every case.
        let pushed = unsafe { ring.submission().push(&sqe) };
        let outcome = match pushed {
            // A fresh ring is empty, so this cannot happen; the entry was not
            // queued either way.
            Err(_) => Outcome::NotSubmitted(io::Error::other(
                "the private io_uring refused its first SQE",
            )),
            Ok(()) => loop {
                match ring.submit_and_wait(1) {
                    Ok(_) => {}
                    // A signal interrupted the wait (or the submission, in
                    // which case the SQE is still queued and goes again).
                    Err(err) if err.kind() == io::ErrorKind::Interrupted => {}
                    Err(err) => {
                        break if ring.submission().is_empty() {
                            Outcome::InFlight(err)
                        } else {
                            Outcome::NotSubmitted(err)
                        };
                    }
                }
                if let Some(cqe) = ring.completion().next() {
                    debug_assert_eq!(cqe.user_data(), GET_FEATURES_USER_DATA);
                    break Outcome::Completed(cqe.result());
                }
            },
        };
        // Closing the ring before the buffer is freed keeps a still-queued
        // SQE from ever reaching the kernel.
        drop(ring);
        let result = match outcome {
            // The driver may still write `buf`: leak it.
            Outcome::InFlight(err) => return Err(err),
            Outcome::NotSubmitted(err) => Err(err),
            Outcome::Completed(result) => Ok(result),
        };
        // SAFETY: `buf` came from `Box::into_raw` above and has not been
        // freed. In both remaining outcomes the kernel is done with it: the
        // CQE of the only operation naming it was reaped (completion is
        // ordered after the driver's copy_to_user, and the CQ tail is read
        // with acquire ordering), or the SQE never left the ring and the
        // ring is closed. No reference to it exists, so ownership returns to
        // a Box that is read and dropped here.
        let features = *unsafe { Box::from_raw(buf) };
        let result = result?;
        if result < 0 {
            return Err(io::Error::from_raw_os_error(-result));
        }
        Ok(features)
    }
}
