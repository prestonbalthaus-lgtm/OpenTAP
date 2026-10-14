//! Malformed stripe summaries, descriptors and hole records: parsing never
//! panics, a summary that parses is self-consistent and identity-checked,
//! and whatever in-place sealing accepts, parsing accepts too. The same
//! bytes are tried in an aligned page and one byte past an 8-byte boundary:
//! the zero-copy views refuse the unaligned copy, the copying header read
//! and in-place sealing give the same result at both offsets.

#![no_main]

use libfuzzer_sys::fuzz_target;
use otap_format::{
    derive_vol_bitmap, seal_summary_in_place, FormatError, FromBytes, HoleRecord, Page16K,
    StripeDescriptor, StripeHdr, StripeSummary, MAP_PAGE_SIZE, STRIPE_HDR_SIZE,
};

/// One page of room whose offset `k` is `k` bytes past an 8-byte boundary.
#[repr(C, align(8))]
struct Shifted([u8; MAP_PAGE_SIZE + 8]);

/// Where the unaligned copy starts.
const SHIFT: usize = 1;

fuzz_target!(|data: &[u8]| {
    // The region as given, at whatever alignment the fuzzer's buffer has.
    let _ = StripeSummary::parse(data);
    let _ = StripeHdr::read(data);

    // The same bytes in an aligned page and one byte past an 8-byte boundary.
    let mut page = Page16K::zeroed();
    let n = data.len().min(MAP_PAGE_SIZE);
    page.0[..n].copy_from_slice(&data[..n]);
    let mut shifted = Shifted([0; MAP_PAGE_SIZE + 8]);
    shifted.0[SHIFT..SHIFT + n].copy_from_slice(&data[..n]);
    let odd = &shifted.0[SHIFT..SHIFT + n];
    assert!(StripeSummary::parse(odd).is_err(), "a view needs alignment");
    if n >= STRIPE_HDR_SIZE {
        assert_eq!(
            StripeHdr::read(&odd[..STRIPE_HDR_SIZE]),
            StripeHdr::read(&page.0[..STRIPE_HDR_SIZE]),
            "the header copy depends on alignment"
        );
        assert!(matches!(
            StripeHdr::parse(&odd[..STRIPE_HDR_SIZE]),
            Err(FormatError::Misaligned { .. })
        ));
    }

    if let Ok(summary) = StripeSummary::parse(&page.0[..n]) {
        let hdr = summary.hdr();
        assert!(hdr.verify_hdr().is_ok());
        assert_eq!(summary.descriptors().len(), hdr.nblocks as usize);
        assert!(derive_vol_bitmap(summary.descriptors()) == *summary.vol_bitmap());
        assert!(summary.summary_bytes() <= n);
        for desc in summary.descriptors() {
            assert!(desc.validate().is_ok());
        }
        let id = hdr.id();
        assert!(StripeSummary::parse_expected(&page.0[..n], &id).is_ok());
        let mut other = id;
        other.cp_nonce ^= 1;
        assert!(StripeSummary::parse_expected(&page.0[..n], &other).is_err());
    }

    // In-place sealing of the same bytes: a success must parse, and sealing
    // works at any alignment, with the same verdict and the same bytes.
    let mut copy = page;
    let sealed = seal_summary_in_place(&mut copy.0[..n]);
    let sealed_odd = seal_summary_in_place(&mut shifted.0[SHIFT..SHIFT + n]);
    assert_eq!(sealed, sealed_odd, "sealing depends on alignment");
    if let Ok(total) = sealed {
        assert!(total <= n);
        assert!(StripeSummary::parse(&copy.0[..total]).is_ok());
        assert!(
            copy.0[..n] == shifted.0[SHIFT..SHIFT + n],
            "sealing wrote other bytes at another alignment"
        );
    }

    // Every 16-byte window as a descriptor and as a hole record.
    for chunk in data.as_chunks::<16>().0 {
        let desc = StripeDescriptor::read_from_bytes(chunk).expect("16 bytes");
        if desc.validate().is_ok() {
            let again = StripeDescriptor::new(
                desc.vol(),
                desc.lba(),
                desc.kind().expect("validated"),
                desc.birth_cp,
                desc.crc32c,
            )
            .expect("a validated descriptor rebuilds");
            assert_eq!(
                again.vol_lba_kind,
                desc.vol_lba_kind & !otap_format::DESC_RSVD_MASK
            );
        }
        let hole = HoleRecord::read_from_bytes(chunk).expect("16 bytes");
        if hole.validate().is_ok() {
            assert!(hole.end_lba().is_some());
            assert!(HoleRecord::new(hole.vol, hole.lba, hole.len).is_ok());
        }
    }
});
