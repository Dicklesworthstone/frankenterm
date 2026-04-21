#![no_main]

use frankenterm_escape_parser::{Action, parser::Parser};
use libfuzzer_sys::Corpus;
use libfuzzer_sys::arbitrary::{Arbitrary, Error, Unstructured};
use libfuzzer_sys::fuzz_target;

const MAX_CSI_SEQUENCES: usize = 12;
const MAX_NOISE_BYTES: usize = 32;
const MAX_PARAM_GROUPS: usize = 8;
const MAX_SUBPARAMS: usize = 4;
const MAX_INTERMEDIATES: usize = 2;
const MAX_MATERIALIZED_BYTES: usize = 8 * 1024;
const NOISE_ALPHABET: &[u8] =
    b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789 \r\n\t\x07\x1b[];:?><=!\"#$%&'()*+,-./";
const COMMON_FINAL_BYTES: &[u8] = b"@ABCDEFGHIKLMNOPSTXZ`abcdefghilmnpqrstuy~";
const PRIVATE_PREFIX_BYTES: &[u8] = b"?!=>";

#[derive(Debug)]
struct NoiseBlob(Vec<u8>);

impl NoiseBlob {
    fn to_bytes(&self) -> &[u8] {
        &self.0
    }
}

impl<'a> Arbitrary<'a> for NoiseBlob {
    fn arbitrary(u: &mut Unstructured<'a>) -> Result<Self, Error> {
        let len = usize::from(u.int_in_range(0_u8..=MAX_NOISE_BYTES as u8)?);
        let mut bytes = Vec::with_capacity(len);
        for _ in 0..len {
            let idx = usize::from(u.int_in_range(0_u8..=(NOISE_ALPHABET.len() - 1) as u8)?);
            bytes.push(NOISE_ALPHABET[idx]);
        }
        Ok(Self(bytes))
    }
}

#[derive(Debug)]
struct ParamGroup {
    head: Option<u16>,
    subparams: Vec<Option<u16>>,
}

impl ParamGroup {
    fn append_to(&self, out: &mut Vec<u8>) {
        if let Some(value) = self.head {
            out.extend_from_slice(value.to_string().as_bytes());
        }

        for subparam in &self.subparams {
            out.push(b':');
            if let Some(value) = subparam {
                out.extend_from_slice(value.to_string().as_bytes());
            }
        }
    }
}

impl<'a> Arbitrary<'a> for ParamGroup {
    fn arbitrary(u: &mut Unstructured<'a>) -> Result<Self, Error> {
        let head = if u.arbitrary::<bool>()? {
            Some(u.int_in_range(0_u16..=9_999_u16)?)
        } else {
            None
        };

        let subparam_len = usize::from(u.int_in_range(0_u8..=MAX_SUBPARAMS as u8)?);
        let mut subparams = Vec::with_capacity(subparam_len);
        for _ in 0..subparam_len {
            let value = if u.arbitrary::<bool>()? {
                Some(u.int_in_range(0_u16..=255_u16)?)
            } else {
                None
            };
            subparams.push(value);
        }

        Ok(Self { head, subparams })
    }
}

#[derive(Debug)]
struct CsiSequence {
    use_c1: bool,
    private_prefix: Option<u8>,
    params: Vec<ParamGroup>,
    intermediates: Vec<u8>,
    final_byte: u8,
}

impl CsiSequence {
    fn to_bytes(&self) -> Vec<u8> {
        let mut out = Vec::new();
        if self.use_c1 {
            out.push(0x9b);
        } else {
            out.extend_from_slice(b"\x1b[");
        }

        if let Some(prefix) = self.private_prefix {
            out.push(prefix);
        }

        for (idx, group) in self.params.iter().enumerate() {
            if idx > 0 {
                out.push(b';');
            }
            group.append_to(&mut out);
        }

        out.extend_from_slice(&self.intermediates);
        out.push(self.final_byte);
        out
    }
}

impl<'a> Arbitrary<'a> for CsiSequence {
    fn arbitrary(u: &mut Unstructured<'a>) -> Result<Self, Error> {
        let private_prefix = if u.arbitrary::<bool>()? {
            let idx = usize::from(u.int_in_range(0_u8..=(PRIVATE_PREFIX_BYTES.len() - 1) as u8)?);
            Some(PRIVATE_PREFIX_BYTES[idx])
        } else {
            None
        };

        let param_len = usize::from(u.int_in_range(0_u8..=MAX_PARAM_GROUPS as u8)?);
        let mut params = Vec::with_capacity(param_len);
        for _ in 0..param_len {
            params.push(ParamGroup::arbitrary(u)?);
        }

        let intermediate_len = usize::from(u.int_in_range(0_u8..=MAX_INTERMEDIATES as u8)?);
        let mut intermediates = Vec::with_capacity(intermediate_len);
        for _ in 0..intermediate_len {
            intermediates.push(u.int_in_range(0x20_u8..=0x2f_u8)?);
        }

        let final_byte = if u.arbitrary::<bool>()? {
            let idx = usize::from(u.int_in_range(0_u8..=(COMMON_FINAL_BYTES.len() - 1) as u8)?);
            COMMON_FINAL_BYTES[idx]
        } else {
            u.int_in_range(0x40_u8..=0x7e_u8)?
        };

        Ok(Self {
            use_c1: u.arbitrary()?,
            private_prefix,
            params,
            intermediates,
            final_byte,
        })
    }
}

#[derive(Debug)]
struct FuzzCase {
    prefix: NoiseBlob,
    sequences: Vec<CsiSequence>,
    separators: Vec<NoiseBlob>,
    suffix: NoiseBlob,
}

impl FuzzCase {
    fn materialize(&self) -> (Vec<u8>, Vec<Vec<u8>>) {
        let mut full = Vec::new();
        let mut chunks = Vec::new();

        if !self.prefix.to_bytes().is_empty() {
            let prefix = self.prefix.to_bytes().to_vec();
            full.extend_from_slice(&prefix);
            chunks.push(prefix);
        }

        for (idx, sequence) in self.sequences.iter().enumerate() {
            let seq_bytes = sequence.to_bytes();
            full.extend_from_slice(&seq_bytes);
            chunks.push(seq_bytes);

            let separator = self.separators[idx].to_bytes();
            if !separator.is_empty() {
                let separator = separator.to_vec();
                full.extend_from_slice(&separator);
                chunks.push(separator);
            }
        }

        if !self.suffix.to_bytes().is_empty() {
            let suffix = self.suffix.to_bytes().to_vec();
            full.extend_from_slice(&suffix);
            chunks.push(suffix);
        }

        (full, chunks)
    }
}

impl<'a> Arbitrary<'a> for FuzzCase {
    fn arbitrary(u: &mut Unstructured<'a>) -> Result<Self, Error> {
        let sequence_len = usize::from(u.int_in_range(1_u8..=MAX_CSI_SEQUENCES as u8)?);
        let mut sequences = Vec::with_capacity(sequence_len);
        let mut separators = Vec::with_capacity(sequence_len);
        for _ in 0..sequence_len {
            sequences.push(CsiSequence::arbitrary(u)?);
            separators.push(NoiseBlob::arbitrary(u)?);
        }

        Ok(Self {
            prefix: NoiseBlob::arbitrary(u)?,
            sequences,
            separators,
            suffix: NoiseBlob::arbitrary(u)?,
        })
    }
}

fn parse_one_shot(bytes: &[u8]) -> Vec<Action> {
    let mut parser = Parser::new();
    parser.parse_as_vec(bytes)
}

fn parse_chunked(chunks: &[Vec<u8>]) -> Vec<Action> {
    let mut parser = Parser::new();
    let mut actions = Vec::new();
    for chunk in chunks {
        parser.parse(chunk, |action| actions.push(action));
    }
    actions
}

fn encode_csi_actions(actions: &[Action]) -> Vec<u8> {
    let mut encoded = Vec::new();
    for action in actions {
        if let Action::CSI(csi) = action {
            encoded.extend_from_slice(csi.to_string().as_bytes());
        }
    }
    encoded
}

fuzz_target!(|case: FuzzCase| -> Corpus {
    let (bytes, chunks) = case.materialize();
    if bytes.is_empty() || bytes.len() > MAX_MATERIALIZED_BYTES {
        return Corpus::Reject;
    }

    let one_shot = parse_one_shot(&bytes);
    let chunked = parse_chunked(&chunks);
    assert_eq!(one_shot, chunked);

    let encoded_csi = encode_csi_actions(&one_shot);
    if !encoded_csi.is_empty() && encoded_csi.len() <= MAX_MATERIALIZED_BYTES {
        let reparsed = parse_one_shot(&encoded_csi);
        for action in reparsed {
            let _ = action.to_string();
        }
    }

    Corpus::Keep
});
