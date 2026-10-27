use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

macro_rules! string_serde {
    ($name:ident) => {
        impl TryFrom<String> for $name {
            type Error = String;

            fn try_from(s: String) -> Result<Self, Self::Error> {
                s.parse()
            }
        }

        impl From<$name> for String {
            fn from(v: $name) -> Self {
                v.to_string()
            }
        }
    };
}

fn hex<T>(s: &str, digits: usize, what: &str) -> Result<T, String>
where
    T: TryFrom<u32>,
{
    let ok = !s.is_empty() && s.len() <= digits && s.chars().all(|c| c.is_ascii_hexdigit());

    ok.then(|| u32::from_str_radix(s, 16).ok())
        .flatten()
        .and_then(|v| T::try_from(v).ok())
        .ok_or_else(|| format!("bad {what} {s:?}"))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct PciAddress {
    pub domain: u16,
    pub bus: u8,
    pub device: u8,
    pub function: u8,
}

impl FromStr for PciAddress {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let bad = || format!("expected a PCI address such as 0000:0b:00.3 (not {s:?})");
        let (rest, function) = s.rsplit_once('.').ok_or_else(bad)?;
        let parts: Vec<&str> = rest.split(':').collect();

        let (domain, bus, device) = match parts.as_slice() {
            [bus, device] => ("0", *bus, *device),
            [domain, bus, device] => (*domain, *bus, *device),
            _ => return Err(bad()),
        };

        let addr = Self {
            domain: hex(domain, 4, "PCI domain")?,
            bus: hex(bus, 2, "PCI bus")?,
            device: hex(device, 2, "PCI device")?,
            function: hex(function, 1, "PCI function")?,
        };

        if addr.device > 0x1f || addr.function > 7 {
            return Err(bad());
        }

        Ok(addr)
    }
}

impl fmt::Display for PciAddress {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{:04x}:{:02x}:{:02x}.{:x}",
            self.domain, self.bus, self.device, self.function
        )
    }
}

string_serde!(PciAddress);

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct UsbId {
    pub vendor: u16,
    pub product: u16,
}

impl FromStr for UsbId {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let (vendor, product) = s
            .split_once(':')
            .ok_or_else(|| format!("expected a USB id such as 046d:c52b (not {s:?})"))?;

        Ok(Self {
            vendor: hex(vendor, 4, "USB vendor id")?,
            product: hex(product, 4, "USB product id")?,
        })
    }
}

impl fmt::Display for UsbId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:04x}:{:04x}", self.vendor, self.product)
    }
}

string_serde!(UsbId);

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct Mac(pub [u8; 6]);

impl Mac {
    pub fn random() -> Self {
        let mut tail = [0_u8; 3];
        let _ = rustix::rand::getrandom(&mut tail, rustix::rand::GetRandomFlags::empty());
        Self([0x52, 0x54, 0x00, tail[0], tail[1], tail[2]])
    }
}

impl FromStr for Mac {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let bad = || format!("expected a MAC address such as 52:54:00:12:34:56 (not {s:?})");
        let parts: Vec<&str> = s.split([':', '-']).collect();

        if parts.len() != 6 || parts.iter().any(|p| p.len() != 2) {
            return Err(bad());
        }

        let mut mac = [0_u8; 6];

        for (out, part) in mac.iter_mut().zip(parts) {
            *out = hex(part, 2, "MAC byte")?;
        }

        if mac[0] & 1 != 0 {
            return Err(format!("{s} is a multicast address"));
        }

        Ok(Self(mac))
    }
}

impl fmt::Display for Mac {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let bytes: Vec<String> = self.0.iter().map(|b| format!("{b:02x}")).collect();
        f.write_str(&bytes.join(":"))
    }
}

string_serde!(Mac);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct Resolution {
    pub width: u32,
    pub height: u32,
}

const MAX_SIDE: u32 = 1 << 14;

impl FromStr for Resolution {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let bad = || format!("expected WIDTHxHEIGHT such as 2560x1440 (not {s:?})");
        let (w, h) = s.split_once(['x', 'X']).ok_or_else(bad)?;
        let (width, height) = (
            w.trim().parse().map_err(|_| bad())?,
            h.trim().parse().map_err(|_| bad())?,
        );

        if !(64..=MAX_SIDE).contains(&width) || !(64..=MAX_SIDE).contains(&height) {
            return Err(format!("{s}: each side is 64 to 16384 pixels"));
        }

        Ok(Self { width, height })
    }
}

impl fmt::Display for Resolution {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}x{}", self.width, self.height)
    }
}

string_serde!(Resolution);
