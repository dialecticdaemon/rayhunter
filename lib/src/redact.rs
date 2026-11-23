//! Removes subscriber and device identities from LTE signalling before a recording is shared.
//!
//! A diag recording contains more than an observer on the air interface could see: the modem
//! logs NAS messages before ciphering, so the IMSI, the IMEISV (sent in every Security Mode
//! Complete when the network asks for it) and the GUTI are in plain text. Paging also carries
//! the identities of other subscribers. Someone who obtains a shared recording could link
//! the user's SIM and device, and an old and a new IMEI after an IMEI change.
//!
//! [`Redactor`] replaces these values with placeholders, while the message types, security
//! algorithms, reject causes, identity request types and cell broadcast stay as they are, so
//! the analyzers give the same result on the redacted messages. Every distinct value gets its
//! own placeholder (1, 2, 3, ...) for the lifetime of one [`Redactor`], so it stays visible that
//! e.g. the same TMSI was paged twice, but placeholders can't be linked across two exports.
//!
//! Replaced:
//! * IMSI: all digits after MCC and MNC (the first 5 digits are kept)
//! * IMEI, IMEISV: all digits
//! * M-TMSI (GUTI, S-TMSI, TMSI): all 32 bits, the MME group/code stay
//! * the UE's IP address in Activate Default EPS Bearer Context Request
//! * the contents of SMS, generic NAS transport and CS service notification (calling number)
//!
//! Kept: cell identity, tracking area, frequencies, timestamps. They are needed to tell which
//! cell raised a warning, and they also reveal where the device was. Limit what is shared to a
//! time window around a warning.
//!
//! The modified messages must still be valid. NAS is rewritten in place with the same length.
//! RRC is decoded to find the identity values, which are then overwritten bit for bit in the
//! original PDU; a message where that isn't unambiguous is dropped rather than shared unredacted.

use std::collections::HashMap;

use crate::diag::{
    CRC_CCITT, HdlcEncapsulatedMessage, MESSAGE_TERMINATOR, Message, MessagesContainer,
};
use crate::gsmtap::parser;
use crate::gsmtap::{GsmtapMessage, GsmtapType, LteNasSubtype, LteRrcSubtype};
use crate::hdlc::{hdlc_decapsulate, hdlc_encapsulate};
use telcom_parser::{decode, lte_rrc};

const PD_EMM: u8 = 0x07;
const PD_ESM: u8 = 0x02;

#[derive(Default)]
pub struct Redactor {
    tmsis: HashMap<u32, u32>,
    imsis: HashMap<Vec<u8>, u64>,
    imeis: HashMap<Vec<u8>, u64>,
}

impl Redactor {
    pub fn new() -> Self {
        Self::default()
    }

    /// Returns the message with identities replaced, or `None` if it may contain identities
    /// that could not be removed safely.
    pub fn redact(&mut self, mut msg: GsmtapMessage) -> Option<GsmtapMessage> {
        match msg.header.gsmtap_type {
            GsmtapType::LteNas(LteNasSubtype::Plain) => {
                self.nas(&mut msg.payload);
                Some(msg)
            }
            GsmtapType::LteNas(LteNasSubtype::Secure) => {
                self.nas(&mut msg.payload);
                Some(msg)
            }
            GsmtapType::LteRrc(subtype) => {
                let payload = self.rrc(subtype, &msg.payload)?;
                msg.payload = payload;
                Some(msg)
            }
            // GSM/UMTS signalling would need its own handling; don't share it
            _ => None,
        }
    }

    /// Returns the part of a diag container that may be written to a recording: LTE RRC and NAS
    /// messages with their identities replaced, and the modem's responses to diag requests.
    /// Everything else is left out, i.e. messages [`Redactor::redact`] can't clean and all other
    /// logs (GSM, UMTS, NR, IP traffic, unknown formats), so that only messages known to be free
    /// of identities reach the disk.
    pub fn scrub_container(&mut self, container: &MessagesContainer) -> MessagesContainer {
        let mut data = Vec::new();
        for msg in &container.messages {
            for frame in msg.data.split_inclusive(|&b| b == MESSAGE_TERMINATOR) {
                if let Some(clean) = self.scrub_frame(frame) {
                    data.extend(clean);
                }
            }
        }
        let messages = if data.is_empty() {
            Vec::new()
        } else {
            vec![HdlcEncapsulatedMessage {
                len: data.len() as u32,
                data,
            }]
        };
        MessagesContainer {
            data_type: container.data_type.clone(),
            num_messages: messages.len() as u32,
            messages,
        }
    }

    fn scrub_frame(&mut self, frame: &[u8]) -> Option<Vec<u8>> {
        match Message::from_hdlc(frame).ok()? {
            Message::Response { .. } => Some(frame.to_vec()),
            msg @ Message::Log { .. } => {
                let (_, gsmtap) = parser::parse(msg).ok()??;
                let original = gsmtap.payload.clone();
                let clean = self.redact(gsmtap)?;
                if clean.payload == original {
                    return Some(frame.to_vec());
                }
                if clean.payload.len() != original.len() {
                    return None;
                }
                // the PDU is copied unchanged into the log packet, so it can be patched in place
                let mut raw = hdlc_decapsulate(frame, &CRC_CCITT).ok()?;
                let at = find_once(&raw, &original)?;
                raw[at..at + original.len()].copy_from_slice(&clean.payload);
                Some(hdlc_encapsulate(&raw, &CRC_CCITT))
            }
        }
    }

    fn tmsi(&mut self, value: u32) -> u32 {
        let next = self.tmsis.len() as u32 + 1;
        *self.tmsis.entry(value).or_insert(next)
    }

    // ---------------------------------------------------------------- NAS

    /// Rewrites a NAS PDU in place. Unknown or truncated structures are zeroed from the point
    /// where parsing stopped, so nothing unparsed is passed on.
    fn nas(&mut self, pdu: &mut [u8]) {
        let Some(&first) = pdu.first() else { return };
        let (security, pd) = (first >> 4, first & 0x0f);
        match pd {
            PD_EMM if (1..=4).contains(&security) => {
                // security protected: header, MAC (4), sequence number (1), then the inner
                // message. Ciphered content is opaque; if it parses as plain NAS (e.g. with the
                // null cipher), redact it as such.
                if pdu.len() > 6 {
                    self.nas(&mut pdu[6..]);
                }
            }
            PD_EMM if security == 0 => self.emm(pdu),
            PD_ESM => self.esm(pdu),
            // service request (security header 12) and others carry no identity
            _ => {}
        }
    }

    fn emm(&mut self, pdu: &mut [u8]) {
        if pdu.len() < 2 {
            return;
        }
        let ok = match pdu[1] {
            // Attach Request: EPS mobile identity, UE network capability, ESM container
            0x41 => self
                .lv_identity(pdu, 3)
                .and_then(|i| skip_lv(pdu, i))
                .and_then(|i| {
                    let end = lve_end(pdu, i)?;
                    self.nas(&mut pdu[i + 2..end]);
                    self.optional_ies(pdu, end)
                }),
            // Attach Accept: result, T3412, TAI list, ESM container
            0x42 => skip_lv(pdu, 4).and_then(|i| {
                let end = lve_end(pdu, i)?;
                self.nas(&mut pdu[i + 2..end]);
                self.optional_ies(pdu, end)
            }),
            // Detach Request: from the UE with an identity, from the network only the type and
            // an optional cause (IEI 0x53)
            0x45 if pdu.get(3) == Some(&0x53) && pdu.len() == 5 => Some(5),
            0x45 => self.lv_identity(pdu, 3),
            // TAU Request: old GUTI, then optional IEs
            0x48 => self
                .lv_identity(pdu, 3)
                .and_then(|i| self.optional_ies(pdu, i)),
            // TAU Accept: optional IEs only
            0x49 => self.optional_ies(pdu, 3),
            // Extended Service Request: M-TMSI, then optional IEs
            0x4c => self
                .lv_identity(pdu, 3)
                .and_then(|i| self.optional_ies(pdu, i)),
            // GUTI Reallocation Command: GUTI, then optional TAI list
            0x50 => self
                .lv_identity(pdu, 2)
                .and_then(|i| self.optional_ies(pdu, i)),
            // Identity Response
            0x56 => self.lv_identity(pdu, 2),
            // Security Mode Complete: optional IMEISV and replayed NAS message
            0x5e => self.optional_ies(pdu, 2),
            // SMS, generic transport, CS service notification (calling number): drop contents
            0x62..=0x64 | 0x68 | 0x69 => Some(zero_from(pdu, 2)),
            // everything else carries no identity
            _ => Some(pdu.len()),
        };
        if ok.is_none() {
            zero_from(pdu, 2);
        }
    }

    fn esm(&mut self, pdu: &mut [u8]) {
        // Activate Default EPS Bearer Context Request: EBI/PD, PTI, type, EPS QoS, APN, PDN address
        if pdu.len() > 3 && pdu[2] == 0xc1 {
            let ok = skip_lv(pdu, 3).and_then(|i| skip_lv(pdu, i)).and_then(|i| {
                let end = i + 1 + *pdu.get(i)? as usize;
                // first octet is the PDN type, the rest the address
                if end > pdu.len() || end < i + 2 {
                    return None;
                }
                pdu[i + 2..end].fill(0);
                // the modem can log more after the address, e.g. the rest of the Attach
                // Accept with the GUTI, so nothing after it is kept
                Some(zero_from(pdu, end))
            });
            if ok.is_none() {
                zero_from(pdu, 3);
            }
        }
    }

    /// Redacts a mobile identity in LV format at `at`; returns the offset after it.
    fn lv_identity(&mut self, pdu: &mut [u8], at: usize) -> Option<usize> {
        let len = *pdu.get(at)? as usize;
        let end = at + 1 + len;
        if end > pdu.len() {
            return None;
        }
        self.identity(&mut pdu[at + 1..end])?;
        Some(end)
    }

    /// Mobile identity (TS 24.008 10.5.1.4) or EPS mobile identity (TS 24.301 9.9.3.12).
    fn identity(&mut self, value: &mut [u8]) -> Option<()> {
        let first = *value.first()?;
        match first & 0x07 {
            // IMSI
            1 => {
                let digits = bcd_digits(value);
                let n = self.imsis.len() as u64 + 1;
                let id = *self.imsis.entry(digits).or_insert(n);
                write_bcd_placeholder(value, 5, id);
            }
            // IMEI, IMEISV (24.008: 2 = IMEI, 3 = IMEISV; 24.301: 3 = IMEI)
            2 | 3 => {
                let digits = bcd_digits(value);
                let n = self.imeis.len() as u64 + 1;
                let id = *self.imeis.entry(digits).or_insert(n);
                write_bcd_placeholder(value, 0, id);
            }
            // TMSI/P-TMSI
            4 if value.len() == 5 => {
                let tmsi = u32::from_be_bytes(value[1..5].try_into().ok()?);
                value[1..5].copy_from_slice(&self.tmsi(tmsi).to_be_bytes());
            }
            // GUTI: PLMN (3), MME group (2), MME code (1), M-TMSI (4)
            6 if value.len() == 11 => {
                let tmsi = u32::from_be_bytes(value[7..11].try_into().ok()?);
                value[7..11].copy_from_slice(&self.tmsi(tmsi).to_be_bytes());
            }
            // no identity
            0 => {}
            _ => {
                value[1..].fill(0);
            }
        }
        Some(())
    }

    /// Walks the optional IEs from `at` to the end. Mobile identities (IEI 0x23, 0x50) are
    /// redacted, NAS containers (0x78, 0x79) are redacted recursively.
    fn optional_ies(&mut self, pdu: &mut [u8], mut at: usize) -> Option<usize> {
        while at < pdu.len() {
            let iei = pdu[at];
            if iei & 0x80 != 0 {
                // type 1 (half-octet IEI and value) or type 2
                at += 1;
                continue;
            }
            if let Some(len) = fixed_tv_len(iei) {
                at += len;
                continue;
            }
            match iei {
                0x78 | 0x79 => {
                    let end = lve_end(pdu, at + 1)?;
                    self.nas(&mut pdu[at + 3..end]);
                    at = end;
                }
                0x23 | 0x50 => at = self.lv_identity(pdu, at + 1)?,
                _ => at = skip_lv(pdu, at + 1)?,
            }
        }
        (at == pdu.len()).then_some(at)
    }

    // ---------------------------------------------------------------- RRC

    /// RRC is decoded only to learn the identity values. They are then replaced bit for bit in
    /// the original PDU: re-encoding would lose extensions the decoder skips. Each old bit pattern
    /// must occur exactly once, and the result must decode again and no longer contain it.
    fn rrc(&mut self, subtype: LteRrcSubtype, payload: &[u8]) -> Option<Vec<u8>> {
        use LteRrcSubtype as L;
        let replacements = match subtype {
            // cell broadcast and messages without UE identities
            L::BcchBch
            | L::BcchDlSch
            | L::DlCcch
            | L::MCCH
            | L::BcchBchMbms
            | L::BcchDlSchBr
            | L::BcchDlSchMbms => return Some(payload.to_vec()),
            L::PCCH => self.pcch(&decode(payload).ok()?),
            L::UlCcch => self.ul_ccch(&decode(payload).ok()?),
            L::UlDcch => self.ul_dcch(&decode(payload).ok()?),
            L::DlDcch => self.dl_dcch(&decode(payload).ok()?),
            _ => return None,
        };
        let redacted = replace_bits(payload, &replacements)?;
        let still_decodes = match subtype {
            L::PCCH => decode::<lte_rrc::PCCH_Message>(&redacted).is_ok(),
            L::UlCcch => decode::<lte_rrc::UL_CCCH_Message>(&redacted).is_ok(),
            L::UlDcch => decode::<lte_rrc::UL_DCCH_Message>(&redacted).is_ok(),
            L::DlDcch => decode::<lte_rrc::DL_DCCH_Message>(&redacted).is_ok(),
            _ => false,
        };
        if !still_decodes {
            log::debug!("redact: dropping RRC message that no longer decodes after redaction");
            return None;
        }
        Some(redacted)
    }

    fn s_tmsi(&mut self, s_tmsi: &lte_rrc::S_TMSI) -> Replacement {
        let mmec: Bits = s_tmsi.mmec.0.iter().map(|b| *b).collect();
        let m_tmsi: Bits = s_tmsi.m_tmsi.0.iter().map(|b| *b).collect();
        let value = m_tmsi.iter().fold(0u32, |acc, b| (acc << 1) | *b as u32);
        let placeholder = self.tmsi(value);
        let new_tmsi: Bits = (0..m_tmsi.len())
            .map(|i| m_tmsi.len() <= 32 && placeholder & (1 << (m_tmsi.len() - 1 - i)) != 0)
            .collect();
        Replacement {
            old: [mmec.clone(), m_tmsi].concat(),
            new: [mmec, new_tmsi].concat(),
        }
    }

    fn imsi(&mut self, imsi: &lte_rrc::IMSI) -> Replacement {
        let digits: Vec<u8> = imsi.0.iter().map(|d| d.0).collect();
        let n = self.imsis.len() as u64 + 1;
        let id = *self.imsis.entry(digits.clone()).or_insert(n);
        let len = digits.len();
        let redacted: Vec<u8> = digits
            .iter()
            .enumerate()
            .map(|(i, d)| {
                if i < 5 {
                    *d
                } else {
                    placeholder_digit(id, len - 1 - i)
                }
            })
            .collect();
        // each digit is an INTEGER (0..9), i.e. 4 bits in UPER
        let to_bits = |ds: &[u8]| -> Bits {
            ds.iter()
                .flat_map(|d| (0..4).map(move |i| d & (8 >> i) != 0))
                .collect()
        };
        Replacement {
            old: to_bits(&digits),
            new: to_bits(&redacted),
        }
    }

    fn rrc_nas(&mut self, nas: &lte_rrc::DedicatedInfoNAS) -> Replacement {
        let mut redacted = nas.0.clone();
        self.nas(&mut redacted);
        Replacement {
            old: bytes_to_bits(&nas.0),
            new: bytes_to_bits(&redacted),
        }
    }

    fn pcch(&mut self, msg: &lte_rrc::PCCH_Message) -> Vec<Replacement> {
        use lte_rrc::{PCCH_MessageType, PCCH_MessageType_c1, PagingUE_Identity};
        let PCCH_MessageType::C1(PCCH_MessageType_c1::Paging(paging)) = &msg.message else {
            return vec![];
        };
        let mut out = vec![];
        for record in paging.paging_record_list.iter().flat_map(|l| l.0.iter()) {
            match &record.ue_identity {
                PagingUE_Identity::S_TMSI(s_tmsi) => out.push(self.s_tmsi(s_tmsi)),
                PagingUE_Identity::Imsi(imsi) => out.push(self.imsi(imsi)),
                _ => {}
            }
        }
        out
    }

    fn ul_ccch(&mut self, msg: &lte_rrc::UL_CCCH_Message) -> Vec<Replacement> {
        use lte_rrc::{
            InitialUE_Identity, RRCConnectionRequestCriticalExtensions, UL_CCCH_MessageType,
            UL_CCCH_MessageType_c1,
        };
        if let UL_CCCH_MessageType::C1(UL_CCCH_MessageType_c1::RrcConnectionRequest(req)) =
            &msg.message
            && let RRCConnectionRequestCriticalExtensions::RrcConnectionRequest_r8(ies) =
                &req.critical_extensions
            && let InitialUE_Identity::S_TMSI(s_tmsi) = &ies.ue_identity
        {
            return vec![self.s_tmsi(s_tmsi)];
        }
        vec![]
    }

    fn ul_dcch(&mut self, msg: &lte_rrc::UL_DCCH_Message) -> Vec<Replacement> {
        use lte_rrc::{
            RRCConnectionSetupCompleteCriticalExtensions as SetupExt,
            RRCConnectionSetupCompleteCriticalExtensions_c1 as SetupC1, UL_DCCH_MessageType,
            UL_DCCH_MessageType_c1 as C1, ULInformationTransfer_r8_IEsDedicatedInfoType as UlInfo,
            ULInformationTransferCriticalExtensions as UlExt,
            ULInformationTransferCriticalExtensions_c1 as UlC1,
        };
        let UL_DCCH_MessageType::C1(c1) = &msg.message else {
            return vec![];
        };
        match c1 {
            C1::RrcConnectionSetupComplete(complete) => {
                if let SetupExt::C1(SetupC1::RrcConnectionSetupComplete_r8(ies)) =
                    &complete.critical_extensions
                {
                    return vec![self.rrc_nas(&ies.dedicated_info_nas)];
                }
            }
            C1::UlInformationTransfer(transfer) => {
                if let UlExt::C1(UlC1::UlInformationTransfer_r8(ies)) =
                    &transfer.critical_extensions
                    && let UlInfo::DedicatedInfoNAS(nas) = &ies.dedicated_info_type
                {
                    return vec![self.rrc_nas(nas)];
                }
            }
            _ => {}
        }
        vec![]
    }

    fn dl_dcch(&mut self, msg: &lte_rrc::DL_DCCH_Message) -> Vec<Replacement> {
        use lte_rrc::{
            DL_DCCH_MessageType, DL_DCCH_MessageType_c1 as C1,
            DLInformationTransfer_r8_IEsDedicatedInfoType as DlInfo,
            DLInformationTransferCriticalExtensions as DlExt,
            DLInformationTransferCriticalExtensions_c1 as DlC1,
            RRCConnectionReconfigurationCriticalExtensions as ReconfExt,
            RRCConnectionReconfigurationCriticalExtensions_c1 as ReconfC1,
        };
        let DL_DCCH_MessageType::C1(c1) = &msg.message else {
            return vec![];
        };
        match c1 {
            C1::DlInformationTransfer(transfer) => {
                if let DlExt::C1(DlC1::DlInformationTransfer_r8(ies)) =
                    &transfer.critical_extensions
                    && let DlInfo::DedicatedInfoNAS(nas) = &ies.dedicated_info_type
                {
                    return vec![self.rrc_nas(nas)];
                }
            }
            C1::RrcConnectionReconfiguration(reconf) => {
                if let ReconfExt::C1(ReconfC1::RrcConnectionReconfiguration_r8(ies)) =
                    &reconf.critical_extensions
                {
                    return ies
                        .dedicated_info_nas_list
                        .iter()
                        .flat_map(|l| l.0.iter())
                        .map(|nas| self.rrc_nas(nas))
                        .collect();
                }
            }
            _ => {}
        }
        vec![]
    }
}

type Bits = Vec<bool>;

struct Replacement {
    old: Bits,
    new: Bits,
}

/// Position of `needle` in `haystack`: at the end, where the log packets keep the PDU, or else
/// its only occurrence.
fn find_once(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || needle.len() > haystack.len() {
        return None;
    }
    if haystack.ends_with(needle) {
        return Some(haystack.len() - needle.len());
    }
    let mut found = haystack
        .windows(needle.len())
        .enumerate()
        .filter(|(_, w)| *w == needle)
        .map(|(i, _)| i);
    match (found.next(), found.next()) {
        (Some(i), None) => Some(i),
        _ => None,
    }
}

fn bytes_to_bits(bytes: &[u8]) -> Bits {
    bytes
        .iter()
        .flat_map(|b| (0..8).map(move |i| b & (0x80 >> i) != 0))
        .collect()
}

fn bits_to_bytes(bits: &[bool]) -> Vec<u8> {
    bits.chunks(8)
        .map(|c| {
            c.iter()
                .enumerate()
                .fold(0u8, |acc, (i, b)| acc | ((*b as u8) << (7 - i)))
        })
        .collect()
}

fn find_bits(haystack: &[bool], needle: &[bool]) -> Vec<usize> {
    if needle.is_empty() || needle.len() > haystack.len() {
        return vec![];
    }
    (0..=haystack.len() - needle.len())
        .filter(|&i| haystack[i..i + needle.len()] == *needle)
        .collect()
}

/// Replaces each changed bit pattern, which must occur exactly once, and checks that none of
/// the old patterns is left afterwards.
fn replace_bits(payload: &[u8], replacements: &[Replacement]) -> Option<Vec<u8>> {
    let mut bits = bytes_to_bits(payload);
    let changed: Vec<&Replacement> = replacements.iter().filter(|r| r.old != r.new).collect();
    for r in &changed {
        let found = find_bits(&bits, &r.old);
        if found.len() != 1 || r.new.len() != r.old.len() {
            log::debug!(
                "redact: dropping RRC message, identity pattern found {} times",
                found.len()
            );
            return None;
        }
        bits[found[0]..found[0] + r.new.len()].copy_from_slice(&r.new);
    }
    if changed.iter().any(|r| !find_bits(&bits, &r.old).is_empty()) {
        log::debug!("redact: dropping RRC message, identity still present after replacement");
        return None;
    }
    Some(bits_to_bytes(&bits))
}

fn skip_lv(pdu: &[u8], at: usize) -> Option<usize> {
    let end = at + 1 + *pdu.get(at)? as usize;
    (end <= pdu.len()).then_some(end)
}

/// End of an LV-E (two-octet length) element starting at `at`.
fn lve_end(pdu: &[u8], at: usize) -> Option<usize> {
    let len = u16::from_be_bytes([*pdu.get(at)?, *pdu.get(at + 1)?]) as usize;
    let end = at + 2 + len;
    (end <= pdu.len()).then_some(end)
}

/// Total length of type 3 (TV) IEs with a fixed length that can follow in EMM messages.
fn fixed_tv_len(iei: u8) -> Option<usize> {
    match iei {
        // T3402, additional information requested, EMM cause, T3423, T3412
        0x17 | 0x53 | 0x59 | 0x5a => Some(2),
        // DRX parameter
        0x5c => Some(3),
        // old P-TMSI signature
        0x19 => Some(4),
        // TAI, LAI
        0x52 | 0x13 => Some(6),
        _ => None,
    }
}

fn zero_from(pdu: &mut [u8], at: usize) -> usize {
    if at < pdu.len() {
        pdu[at..].fill(0);
    }
    pdu.len()
}

/// The BCD digits of a mobile identity value (first digit in the high nibble of octet 1).
fn bcd_digits(value: &[u8]) -> Vec<u8> {
    let mut digits = vec![value[0] >> 4];
    for b in &value[1..] {
        digits.push(b & 0x0f);
        digits.push(b >> 4);
    }
    digits.retain(|d| *d != 0x0f);
    digits
}

fn placeholder_digit(id: u64, position_from_end: usize) -> u8 {
    ((id / 10u64.saturating_pow(position_from_end as u32)) % 10) as u8
}

/// Overwrites all BCD digits after the first `keep` with the decimal placeholder `id`,
/// right-aligned and zero-padded, keeping the filler nibble.
fn write_bcd_placeholder(value: &mut [u8], keep: usize, id: u64) {
    // digit slots in order: high nibble of octet 0, then low/high of the following octets
    let mut slots: Vec<(usize, bool)> = vec![(0, true)];
    for i in 1..value.len() {
        slots.push((i, false));
        slots.push((i, true));
    }
    slots.retain(|&(i, high)| {
        let nibble = if high { value[i] >> 4 } else { value[i] & 0x0f };
        nibble != 0x0f
    });
    let total = slots.len();
    for (n, &(i, high)) in slots.iter().enumerate().skip(keep) {
        let digit = placeholder_digit(id, total - 1 - n);
        if high {
            value[i] = (value[i] & 0x0f) | (digit << 4);
        } else {
            value[i] = (value[i] & 0xf0) | digit;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn redact_nas(pdu: &[u8]) -> Vec<u8> {
        let mut r = Redactor::new();
        let mut pdu = pdu.to_vec();
        r.nas(&mut pdu);
        pdu
    }

    #[test]
    fn identity_response_imsi_keeps_plmn_only() {
        // Identity Response, IMSI 001 01 0123456789
        let pdu = [
            0x07, 0x56, 0x08, 0x09, 0x10, 0x10, 0x10, 0x32, 0x54, 0x76, 0x98,
        ];
        let out = redact_nas(&pdu);
        assert_eq!(out.len(), pdu.len());
        assert_eq!(
            bcd_digits(&out[3..]),
            vec![0, 0, 1, 0, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1]
        );
    }

    #[test]
    fn security_mode_complete_imeisv_is_replaced() {
        // Security Mode Complete with IMEISV (IEI 0x23, 9 octets, type 3)
        let pdu = [
            0x07, 0x5e, 0x23, 0x09, 0x33, 0x55, 0x55, 0x55, 0x55, 0x55, 0x55, 0x55, 0xf5,
        ];
        let out = redact_nas(&pdu);
        let digits = bcd_digits(&out[4..]);
        assert_eq!(digits.len(), 16);
        assert!(digits[..15].iter().all(|d| *d == 0));
        assert_eq!(digits[15], 1);
        assert_eq!(out[12] & 0xf0, 0xf0, "filler kept");
    }

    #[test]
    fn same_tmsi_same_placeholder() {
        let mut r = Redactor::new();
        // GUTI Reallocation Command with GUTI, M-TMSI c0000013
        let guti = [
            0x07, 0x50, 0x0b, 0xf6, 0x99, 0xf9, 0x07, 0x00, 0x02, 0x01, 0xc0, 0x00, 0x00, 0x13,
        ];
        let mut a = guti.to_vec();
        let mut b = guti.to_vec();
        r.nas(&mut a);
        r.nas(&mut b);
        assert_eq!(&a[10..14], &[0, 0, 0, 1]);
        assert_eq!(a, b);
        assert_eq!(&a[3..10], &guti[3..10], "PLMN, MME group and code kept");
    }

    #[test]
    fn attach_request_with_imsi_in_security_mode_complete_container() {
        // Security Mode Complete replaying an Attach Request with IMSI (IEI 0x79, LV-E)
        let attach = [
            0x07, 0x41, 0x71, 0x08, 0x09, 0x10, 0x10, 0x10, 0x32, 0x54, 0x76, 0x98, 0x02, 0xe0,
            0xe0, 0x00, 0x00,
        ];
        let mut pdu = vec![0x07, 0x5e, 0x79, 0x00, attach.len() as u8];
        pdu.extend_from_slice(&attach);
        let out = redact_nas(&pdu);
        assert_eq!(&out[..8], &pdu[..8]);
        assert_eq!(bcd_digits(&out[9..17])[5..], [0, 0, 0, 0, 0, 0, 0, 0, 0, 1]);
    }

    #[test]
    fn cs_service_notification_calling_number_is_dropped() {
        // CS Service Notification (0x64): paging identity, then CLI IE 0x60 with a calling number
        let pdu = [0x07, 0x64, 0x00, 0x60, 0x04, 0x91, 0x21, 0x43, 0x65];
        let out = redact_nas(&pdu);
        assert_eq!(&out[..2], &pdu[..2]);
        assert!(out[2..].iter().all(|b| *b == 0));
    }

    #[test]
    fn unparseable_emm_is_zeroed() {
        // TAU Request whose identity length runs past the end
        let out = redact_nas(&[0x07, 0x48, 0x01, 0x20, 0xf6, 0x99]);
        assert_eq!(out, vec![0x07, 0x48, 0, 0, 0, 0]);
    }

    #[test]
    fn ue_ip_address_is_zeroed() {
        // Activate Default EPS Bearer Context Request: QoS LV, APN LV "a", PDN address IPv4
        let pdu = [
            0x52, 0x01, 0xc1, 0x01, 0x09, 0x02, 0x01, 0x61, 0x05, 0x01, 0xc0, 0xa8, 0x64, 0x04,
        ];
        let out = redact_nas(&pdu);
        assert_eq!(&out[..10], &pdu[..10]);
        assert_eq!(&out[10..], &[0, 0, 0, 0]);
    }

    #[test]
    fn replace_bits_unaligned_and_unambiguous_only() {
        // pattern 1011_0011 starting at bit 3 of 0b000_10110_011_00000
        let payload = [0b0001_0110, 0b0110_0000];
        let r = Replacement {
            old: bytes_to_bits(&[0b1011_0011]),
            new: bytes_to_bits(&[0b0000_0001]),
        };
        assert_eq!(
            replace_bits(&payload, &[r]),
            Some(vec![0b0000_0000, 0b0010_0000])
        );
        // a pattern that occurs twice is not replaced
        let r = Replacement {
            old: vec![true, false],
            new: vec![false, false],
        };
        assert_eq!(replace_bits(&[0b1010_0000], &[r]), None);
    }

    // diag log frame (command 0x10) with the given log code and body, as the modem sends it
    fn log_frame(log_type: u16, body: &[u8]) -> Vec<u8> {
        let inner_length = (12 + body.len()) as u16;
        let mut raw = vec![0x10, 0x00];
        raw.extend(inner_length.to_le_bytes());
        raw.extend(inner_length.to_le_bytes());
        raw.extend(log_type.to_le_bytes());
        raw.extend([0u8; 8]);
        raw.extend(body);
        hdlc_encapsulate(&raw, &CRC_CCITT)
    }

    fn nas_frame(log_type: u16, pdu: &[u8]) -> Vec<u8> {
        let mut body = vec![1, 0, 0, 0];
        body.extend(pdu);
        log_frame(log_type, &body)
    }

    fn container(frames: &[Vec<u8>]) -> MessagesContainer {
        let data = frames.concat();
        MessagesContainer {
            data_type: crate::diag::DataType::UserSpace,
            num_messages: 1,
            messages: vec![HdlcEncapsulatedMessage {
                len: data.len() as u32,
                data,
            }],
        }
    }

    fn scrubbed_frames(frames: &[Vec<u8>]) -> Vec<Vec<u8>> {
        let out = Redactor::new().scrub_container(&container(frames));
        out.messages
            .iter()
            .flat_map(|m| m.data.split_inclusive(|&b| b == MESSAGE_TERMINATOR))
            .map(|f| f.to_vec())
            .collect()
    }

    // Identity Response, IMSI 001 01 0123456789
    const IDENTITY_RESPONSE: [u8; 11] = [
        0x07, 0x56, 0x08, 0x09, 0x10, 0x10, 0x10, 0x32, 0x54, 0x76, 0x98,
    ];

    #[test]
    fn scrubbed_nas_log_has_no_imsi_and_stays_valid() {
        let frames = scrubbed_frames(&[nas_frame(0xb0ed, &IDENTITY_RESPONSE)]);
        assert_eq!(frames.len(), 1);
        let raw = hdlc_decapsulate(&frames[0], &CRC_CCITT).expect("valid frame and CRC");
        assert!(
            find_once(&raw, &IDENTITY_RESPONSE[5..]).is_none(),
            "MSIN still present"
        );
        let (_, gsmtap) = parser::parse(Message::from_hdlc(&frames[0]).unwrap())
            .unwrap()
            .unwrap();
        assert_eq!(gsmtap.payload.len(), IDENTITY_RESPONSE.len());
        assert_eq!(
            bcd_digits(&gsmtap.payload[3..]),
            vec![0, 0, 1, 0, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1]
        );
    }

    #[test]
    fn nas_log_without_identity_is_kept_unchanged() {
        // Attach Complete with an empty ESM container
        let frame = nas_frame(0xb0ed, &[0x07, 0x43, 0x00, 0x00]);
        assert_eq!(scrubbed_frames(std::slice::from_ref(&frame)), vec![frame]);
    }

    #[test]
    fn logs_that_cannot_be_scrubbed_are_left_out() {
        // GSM RR signalling (0x512f), an ML1 log the parser doesn't convert (0xb193) and a
        // frame with a broken CRC
        let gsm = log_frame(0x512f, &[0x00, 0x00, 0x03, 0x06, 0x21, 0x00]);
        let ml1 = log_frame(0xb193, &[0; 16]);
        let mut broken = nas_frame(0xb0ed, &IDENTITY_RESPONSE);
        let crc = broken.len() - 2;
        broken[crc] ^= 0xff;
        let kept = nas_frame(0xb0ed, &[0x07, 0x43, 0x00, 0x00]);
        assert_eq!(
            scrubbed_frames(&[gsm, ml1, broken, kept.clone()]),
            vec![kept]
        );
    }

    #[test]
    fn diag_responses_are_kept() {
        // response to a SetMask request (opcode 115, subopcode 3, status 0)
        let mut raw = vec![115, 0, 0, 0, 3, 0, 0, 0, 0, 0, 0, 0];
        raw.extend([0u8; 8]);
        let frame = hdlc_encapsulate(&raw, &CRC_CCITT);
        assert!(matches!(
            Message::from_hdlc(&frame),
            Ok(Message::Response { .. })
        ));
        assert_eq!(scrubbed_frames(std::slice::from_ref(&frame)), vec![frame]);
    }

    #[test]
    fn default_bearer_request_drops_everything_after_the_address() {
        // Activate Default EPS Bearer Context Request as the EM060K logs it (0xb0e2): QoS, APN
        // "internet", PDN address, APN-AMBR, then the GUTI of the Attach Accept, M-TMSI c0000013
        let esm = [
            0x52, 0x04, 0xc1, 0x01, 0x09, 0x09, 0x08, b'i', b'n', b't', b'e', b'r', b'n', b'e',
            b't', 0x05, 0x01, 0x0a, 0x2d, 0x00, 0x02, 0x5e, 0x02, 0xfe, 0xfe, 0x50, 0x0b, 0xf6,
            0x99, 0xf9, 0x07, 0x00, 0x02, 0x01, 0xc0, 0x00, 0x00, 0x13,
        ];
        let frames = scrubbed_frames(&[nas_frame(0xb0e2, &esm)]);
        assert_eq!(frames.len(), 1);
        let out = hdlc_decapsulate(&frames[0], &CRC_CCITT).unwrap();
        assert!(
            find_once(&out, &[0xc0, 0x00, 0x00, 0x13]).is_none(),
            "M-TMSI left"
        );
        assert!(
            find_once(&out, &[0x0a, 0x2d, 0x00, 0x02]).is_none(),
            "address left"
        );
        assert!(find_once(&out, b"internet").is_some(), "APN kept");
    }

    #[test]
    fn find_once_prefers_the_end_and_rejects_ambiguity() {
        assert_eq!(find_once(&[1, 2, 3, 1, 2], &[1, 2]), Some(3));
        assert_eq!(find_once(&[1, 2, 9, 1, 2, 9], &[1, 2]), None);
        assert_eq!(find_once(&[5, 1, 2, 7], &[1, 2]), Some(1));
        assert_eq!(find_once(&[1], &[]), None);
    }
}
