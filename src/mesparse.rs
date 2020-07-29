use crate::cipher::TuyaCipher;
use crate::crc::crc;
use crate::error::ErrorKind;
use hex::FromHex;
use log::{debug, error};
use nom::{
    bytes::complete::tag,
    combinator::{map, peek, recognize},
    multi::{length_data, many1},
    number::complete::be_u32,
    sequence::tuple,
    IResult,
};

use num_derive::{FromPrimitive, ToPrimitive};
use num_traits::{FromPrimitive, ToPrimitive};
use std::cmp::PartialEq;
use std::str::FromStr;

pub type Result<T> = std::result::Result<T, ErrorKind>;

const UDP_KEY: &str = "yGAdlopoPVldABfn";

lazy_static! {
    static ref PREFIX_BYTES: [u8; 4] = <[u8; 4]>::from_hex("000055AA").unwrap();
    static ref SUFFIX_BYTES: [u8; 4] = <[u8; 4]>::from_hex("0000AA55").unwrap();
}

/// Human readable definitions of command bytes.
#[allow(dead_code)]
#[derive(Debug, FromPrimitive, ToPrimitive, Clone, PartialEq)]
pub enum CommandType {
    Udp = 0,
    ApConfig = 1,
    Active = 2,
    Bind = 3,
    RenameGw = 4,
    RenameDevice = 5,
    Unbind = 6,
    Control = 7,
    Status = 8,
    HeartBeat = 9,
    DpQuery = 10,
    QueryWifi = 11,
    TokenBind = 12,
    ControlNew = 13,
    EnableWifi = 14,
    DpQueryNew = 16,
    SceneExecute = 17,
    UdpNew = 19,
    ApConfigNew = 20,
    LanGwActive = 240,
    LanSubDevRequest = 241,
    LanDeleteSubDev = 242,
    LanReportSubDev = 243,
    LanScene = 244,
    LanPublishCloudConfig = 245,
    LanPublishAppConfig = 246,
    LanExportAppConfig = 247,
    LanPublishScenePanel = 248,
    LanRemoveGw = 249,
    LanCheckGwUpdate = 250,
    LanGwUpdate = 251,
    LanSetGwChannel = 252,
    Error = 255,
}

#[derive(Debug, PartialEq, Clone)]
pub(crate) enum TuyaVersion {
    ThreeOne,
    ThreeThree,
}

impl TuyaVersion {
    pub fn as_bytes(&self) -> &[u8] {
        match &self {
            TuyaVersion::ThreeOne => "3.1".as_bytes(),
            TuyaVersion::ThreeThree => "3.3".as_bytes(),
        }
    }
}

impl FromStr for TuyaVersion {
    type Err = ErrorKind;

    fn from_str(s: &str) -> Result<Self> {
        let version: Vec<&str> = s.split(".").collect();
        if version.len() > 1 && version[0].ends_with("3") {
            if version[1] == "1" {
                return Ok(TuyaVersion::ThreeOne);
            } else if version[1] == "3" {
                return Ok(TuyaVersion::ThreeThree);
            }
            return Err(
                ErrorKind::VersionError(version[0].to_string(), version[1].to_string()).into(),
            );
        }
        Err(ErrorKind::VersionError("Unknown".to_string(), "Unknown".to_string()).into())
    }
}

#[derive(Debug, PartialEq)]
pub struct Message {
    payload: Vec<u8>,
    command: Option<CommandType>,
    seq_nr: Option<u32>,
    ret_code: Option<u8>,
}

impl Message {
    pub fn new(payload: &[u8], command: CommandType, seq_nr: Option<u32>) -> Message {
        Message {
            payload: payload.to_vec(),
            command: Some(command),
            seq_nr,
            ret_code: None,
        }
    }
}

pub struct MessageParser {
    version: TuyaVersion,
    cipher: TuyaCipher,
}

impl MessageParser {
    pub fn create(ver: &str, key: Option<&str>) -> Result<MessageParser> {
        let version = TuyaVersion::from_str(ver)?;
        let key = verify_key(key)?;
        let cipher = TuyaCipher::create(&key, version.clone());
        Ok(MessageParser { version, cipher })
    }

    pub fn encode(&self, mes: &Message, encrypt: bool) -> Result<Vec<u8>> {
        let mut encoded: Vec<u8> = vec![];
        encoded.extend_from_slice(&*PREFIX_BYTES);
        match mes.seq_nr {
            Some(nr) => encoded.extend(&nr.to_be_bytes()),
            None => encoded.extend(&0_u32.to_be_bytes()),
        }
        let command = mes.command.clone().ok_or(ErrorKind::CommandTypeMissing)?;
        encoded.extend([0, 0, 0, command.to_u8().unwrap()].iter());
        let payload = match self.version {
            TuyaVersion::ThreeOne => {
                if encrypt {
                    self.create_payload_with_header(&mes.payload)?
                } else {
                    mes.payload.clone()
                }
            }
            TuyaVersion::ThreeThree => {
                if let Some(CommandType::DpQuery) = mes.command {
                    self.cipher.encrypt(&mes.payload)?
                } else {
                    self.create_payload_with_header(&mes.payload)?
                }
            }
        };
        encoded.extend((payload.len() as u32 + 8_u32).to_be_bytes().iter());
        encoded.extend(payload);
        encoded.extend(crc(&encoded).to_be_bytes().iter());
        encoded.extend_from_slice(&*SUFFIX_BYTES);
        debug!("{:?}", encoded);

        Ok(encoded)
    }

    fn create_payload_with_header(&self, payload: &[u8]) -> Result<Vec<u8>> {
        let mut payload_with_header = Vec::new();
        payload_with_header.extend(self.version.as_bytes());
        match self.version {
            TuyaVersion::ThreeOne => payload_with_header.extend(vec![0; 12]),
            TuyaVersion::ThreeThree => payload_with_header.extend(self.cipher.md5(payload)),
        }
        payload_with_header.extend(self.cipher.encrypt(&payload)?);
        Ok(payload_with_header)
    }

    pub fn parse(&self, buf: &[u8]) -> Result<Vec<Message>> {
        let (buf, messages) = self.parse_messages(buf).map_err(|err| match err {
            nom::Err::Error((_, e)) => ErrorKind::ParseError(e),
            nom::Err::Incomplete(_) => ErrorKind::ParsingIncomplete,
            nom::Err::Failure((_, e)) if e == nom::error::ErrorKind::Verify => {
                ErrorKind::ParseError(e)
            }
            nom::Err::Failure((_, e)) => ErrorKind::ParseError(e),
        })?;
        if !buf.is_empty() {
            return Err(ErrorKind::BufferNotCompletelyParsedError);
        }
        Ok(messages)
    }

    fn parse_messages<'a>(&self, orig_buf: &'a [u8]) -> IResult<&'a [u8], Vec<Message>> {
        // TODO: can this be statically initialized??
        let be_u32_minus4 = map(be_u32, |n: u32| n - 4);
        let (buf, vec) = many1(tuple((
            tag(*PREFIX_BYTES),
            be_u32,
            be_u32,
            length_data(be_u32_minus4),
            tag(*SUFFIX_BYTES),
        )))(orig_buf)?;
        let mut messages = vec![];
        for (_, seq_nr, command, recv_data, _) in vec {
            // check if the recv_data contains a return code
            let (recv_data, maybe_retcode) = peek(be_u32)(recv_data)?;
            let (recv_data, ret_code, ret_len) = if maybe_retcode & 0xFFFFFF00 == 0 {
                // Has a return code
                let (recv_data, ret_code) = recognize(be_u32)(recv_data)?;
                (recv_data, Some(ret_code[3]), 4_usize)
            } else {
                // Has no return code
                (recv_data, None, 0_usize)
            };
            let (payload, rc) = recv_data.split_at(recv_data.len() - 4);
            let recv_crc = u32::from_be_bytes([rc[0], rc[1], rc[2], rc[3]]);
            if crc(&orig_buf[0..recv_data.len() + 12 + ret_len]) != recv_crc {
                error!(
                    "Found CRC: {:#x}, Expected CRC: {:#x}",
                    recv_crc,
                    crc(&orig_buf[0..recv_data.len() + 12 + ret_len])
                );
                // I hijack the ErrorKind::Verify here to propagate a CRC error
                // TODO: should probably create and use a special CRC error here
                return Err(nom::Err::Failure((rc, nom::error::ErrorKind::Verify)));
            }

            let payload = self.try_decrypt(payload);
            let message = Message {
                payload,
                command: FromPrimitive::from_u32(command).or(None),
                seq_nr: Some(seq_nr),
                ret_code,
            };
            messages.push(message);
        }
        Ok((buf, messages))
    }

    fn try_decrypt<'a>(&self, payload: &'a [u8]) -> Vec<u8> {
        match self.cipher.decrypt(payload) {
            Ok(decrypted) => decrypted,
            Err(_) => payload.to_vec(),
        }
    }
}

fn verify_key(key: Option<&str>) -> Result<Vec<u8>> {
    match key {
        Some(key) => {
            if key.len() == 16 {
                return Ok(key.as_bytes().to_vec());
            } else {
                return Err(ErrorKind::KeyLength(key.len()));
            }
        }
        None => {
            let default_key = md5::compute(UDP_KEY).0;
            return Ok(default_key.to_vec());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn test_key_length_is_16() {
        let key = Some("0123456789ABCDEF");
        assert!(verify_key(key).is_ok());
    }

    #[test]
    fn test_key_lenght_not_16_gives_error() {
        let bad_key = Some("13579BDF");
        assert!(verify_key(bad_key).is_err());
    }

    #[test]
    fn test_parse_mqttversion() {
        let version = TuyaVersion::from_str("3.1").unwrap();
        assert_eq!(version, TuyaVersion::ThreeOne);

        let version2 = TuyaVersion::from_str("ver3.3").unwrap();
        assert_eq!(version2, TuyaVersion::ThreeThree);

        assert!(TuyaVersion::from_str("3.4").is_err());
    }

    #[test]
    fn test_parse_messages() {
        let packet =
            hex::decode("000055aa00000000000000090000000c00000000b051ab030000aa55").unwrap();
        let expected = Message {
            command: Some(CommandType::HeartBeat),
            payload: Vec::new(),
            seq_nr: Some(0),
            ret_code: Some(0),
        };
        let mp = MessageParser::create("3.1", None).unwrap();
        let (buf, messages) = mp.parse_messages(&packet).unwrap();
        assert_eq!(messages[0], expected);
        assert_eq!(buf, &[] as &[u8]);
    }

    #[test]
    fn test_parse_messages_with_payload() {
        let packet =
            hex::decode("000055aa00000000000000070000005b00000000332e33290725773ab6c9a1184b38fc8f439ca4abe8d958d12d34a39a6bf230c7ed59d77c0499f0f543640ae8a029957a55b39b5d0213726b385ece93bf5ae2330f71be0f0390f4075008032a624750cd3bfb680000aa55").unwrap();
        let expected = Message {
            command: Some(CommandType::Control),
            payload: r#"{"dev_id":"46052834d8f15b92e53b","dps":{"1":true}}"#
                .as_bytes()
                .to_owned(),
            seq_nr: Some(0),
            ret_code: Some(0),
        };
        let mp = MessageParser::create("3.3", None).unwrap();
        let (buf, messages) = mp.parse_messages(&packet).unwrap();
        assert_eq!(messages[0], expected);
        assert_eq!(buf, &[] as &[u8]);
    }

    #[test]
    fn test_parse_data_format_error() {
        let packet =
            hex::decode("000055aa00000000000000070000003b00000001332e33d504910232d355a59ed1f6ed1f4a816a1e8e30ed09987c020ae45d72c70592bb233c79c43a5b9ae49b6ead38725deb520000aa55").unwrap();
        let expected = Message {
            command: Some(CommandType::Control),
            payload: "data format error".as_bytes().to_owned(),
            seq_nr: Some(0),
            ret_code: Some(1),
        };
        let mp = MessageParser::create("3.3", None).unwrap();
        let (buf, messages) = mp.parse_messages(&packet).unwrap();
        assert_eq!(messages[0], expected);
        assert_eq!(buf, &[] as &[u8]);
    }

    #[test]
    fn test_parse_double_messages() {
        let packet =
            hex::decode("000055aa00000000000000090000000c00000000b051ab030000aa55000055aa000000000000000a0000000c00000000b051ab030000aa55").unwrap();
        let expected = vec![
            Message {
                command: Some(CommandType::HeartBeat),
                payload: Vec::new(),
                seq_nr: Some(0),
                ret_code: Some(0),
            },
            Message {
                command: Some(CommandType::DpQuery),
                payload: Vec::new(),
                seq_nr: Some(0),
                ret_code: Some(0),
            },
        ];
        let mp = MessageParser::create("3.1", None).unwrap();
        let (buf, messages) = mp.parse_messages(&packet).unwrap();
        assert_eq!(messages[0], expected[0]);
        assert_eq!(messages[1], expected[1]);
        assert_eq!(buf, &[] as &[u8]);
    }

    #[test]
    fn test_encode_with_and_without_encryption_and_version_three_one() {
        let payload = r#"{"devId":"002004265ccf7fb1b659","dps":{"1":true,"2":0}}"#
            .as_bytes()
            .to_owned();
        let mes = Message {
            command: Some(CommandType::DpQuery),
            payload,
            seq_nr: Some(0),
            ret_code: Some(0),
        };
        let parser = MessageParser::create("3.1", None).unwrap();
        let encrypted = parser.encode(&mes, true).unwrap();
        let unencrypted = parser.encode(&mes, false).unwrap();
        // Only encrypt 3.1 if the flag is set
        assert_ne!(encrypted, unencrypted);
    }

    #[test]
    fn test_encode_with_and_without_encryption_and_version_three_three() {
        let payload = r#"{"devId":"002004265ccf7fb1b659","dps":{"1":true,"2":0}}"#
            .as_bytes()
            .to_owned();
        let mes = Message {
            command: Some(CommandType::DpQuery),
            payload,
            seq_nr: Some(0),
            ret_code: Some(0),
        };
        let parser = MessageParser::create("3.3", None).unwrap();
        let encrypted = parser.encode(&mes, true).unwrap();
        let unencrypted = parser.encode(&mes, false).unwrap();
        // Always encrypt 3.3, no matter what the flag is
        assert_eq!(encrypted, unencrypted);
    }
}
