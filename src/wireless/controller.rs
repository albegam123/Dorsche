use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail, ensure};

const ALTSETTING_KEY: &str = "bluetooth.hfp.linux_hci_driver_msbc_altsetting";
const PACKET_SIZE_KEY: &str = "bluetooth.hfp.linux_hci_driver_msbc_packet_size";

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UsbControllerIdentity {
    pub hci: u32,
    pub vendor: u16,
    pub product: u16,
    pub bcd_device: Option<u16>,
    pub sysfs_device: PathBuf,
}

impl UsbControllerIdentity {
    pub fn usb_id(&self) -> String {
        format!("{:04x}:{:04x}", self.vendor, self.product)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct HfpTransportQuirk {
    pub name: &'static str,
    pub vendor: u16,
    pub product: u16,
    pub msbc_altsetting: u8,
    pub msbc_packet_size: u16,
}

const HFP_TRANSPORT_QUIRKS: &[HfpTransportQuirk] = &[
    HfpTransportQuirk {
        name: "csr8510-a10",
        vendor: 0x0a12,
        product: 0x0001,
        msbc_altsetting: 1,
        msbc_packet_size: 48,
    },
    HfpTransportQuirk {
        name: "realtek-rtl8761bu",
        vendor: 0x2b89,
        product: 0x8761,
        msbc_altsetting: 3,
        msbc_packet_size: 72,
    },
];

pub fn hfp_transport_quirk(identity: &UsbControllerIdentity) -> Option<HfpTransportQuirk> {
    HFP_TRANSPORT_QUIRKS
        .iter()
        .copied()
        .find(|quirk| quirk.vendor == identity.vendor && quirk.product == identity.product)
}

pub fn detect_usb_controller(sysfs_root: &Path, hci: u32) -> Result<UsbControllerIdentity> {
    let device_link = sysfs_root.join(format!("hci{hci}/device"));
    let device = device_link
        .canonicalize()
        .with_context(|| format!("resolve {}", device_link.display()))?;

    for candidate in device.ancestors() {
        let vendor_path = candidate.join("idVendor");
        let product_path = candidate.join("idProduct");
        if !vendor_path.is_file() || !product_path.is_file() {
            continue;
        }
        return Ok(UsbControllerIdentity {
            hci,
            vendor: read_hex_u16(&vendor_path)?,
            product: read_hex_u16(&product_path)?,
            bcd_device: read_optional_hex_u16(&candidate.join("bcdDevice"))?,
            sysfs_device: candidate.to_owned(),
        });
    }

    bail!(
        "{} is not backed by a discoverable USB device",
        device_link.display()
    )
}

pub fn render_sysprops_with_quirk(current: &str, quirk: HfpTransportQuirk) -> String {
    let mut lines: Vec<String> = current.lines().map(str::to_owned).collect();
    let section_start = lines.iter().position(|line| line.trim() == "[Sysprops]");
    let start = match section_start {
        Some(index) => index + 1,
        None => {
            if !lines.is_empty() && !lines.last().is_some_and(String::is_empty) {
                lines.push(String::new());
            }
            lines.push("[Sysprops]".to_owned());
            lines.len()
        }
    };
    let mut end = lines[start..]
        .iter()
        .position(|line| {
            let line = line.trim();
            line.starts_with('[') && line.ends_with(']')
        })
        .map_or(lines.len(), |offset| start + offset);

    lines.drain_filter_range(start, end, |line| {
        let key = line.split_once('=').map(|(key, _)| key.trim());
        matches!(key, Some(ALTSETTING_KEY | PACKET_SIZE_KEY))
    });
    end = lines[start..]
        .iter()
        .position(|line| {
            let line = line.trim();
            line.starts_with('[') && line.ends_with(']')
        })
        .map_or(lines.len(), |offset| start + offset);
    lines.insert(end, format!("{ALTSETTING_KEY}={}", quirk.msbc_altsetting));
    lines.insert(
        end + 1,
        format!("{PACKET_SIZE_KEY}={}", quirk.msbc_packet_size),
    );
    format!("{}\n", lines.join("\n"))
}

pub fn apply_hfp_transport_quirk(path: &Path, quirk: HfpTransportQuirk) -> Result<bool> {
    validate_quirk(quirk)?;
    let current = match fs::read_to_string(path) {
        Ok(current) => current,
        Err(error) if error.kind() == io::ErrorKind::NotFound => String::new(),
        Err(error) => return Err(error).with_context(|| format!("read {}", path.display())),
    };
    let updated = render_sysprops_with_quirk(&current, quirk);
    if updated == current {
        return Ok(false);
    }
    atomic_replace(path, updated.as_bytes())?;
    Ok(true)
}

fn validate_quirk(quirk: HfpTransportQuirk) -> Result<()> {
    ensure!(
        (1..=6).contains(&quirk.msbc_altsetting),
        "invalid mSBC altsetting {}",
        quirk.msbc_altsetting
    );
    ensure!(
        matches!(quirk.msbc_packet_size, 24 | 48 | 60 | 72),
        "invalid mSBC packet size {}",
        quirk.msbc_packet_size
    );
    Ok(())
}

fn atomic_replace(path: &Path, contents: &[u8]) -> Result<()> {
    let parent = path
        .parent()
        .with_context(|| format!("{} has no parent directory", path.display()))?;
    fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    let temp = parent.join(format!(
        ".{}.dorsche-{}.tmp",
        path.file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("sysprops"),
        std::process::id()
    ));
    let old_metadata = fs::metadata(path).ok();
    let result = (|| -> Result<()> {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp)
            .with_context(|| format!("create {}", temp.display()))?;
        file.write_all(contents)
            .with_context(|| format!("write {}", temp.display()))?;
        let mode = old_metadata
            .as_ref()
            .map_or(0o640, |metadata| metadata.mode() & 0o7777);
        file.set_permissions(fs::Permissions::from_mode(mode))?;
        if let Some(metadata) = &old_metadata {
            set_owner(&file, metadata.uid(), metadata.gid())?;
        }
        file.sync_all()?;
        fs::rename(&temp, path).with_context(|| format!("replace {}", path.display()))?;
        File::open(parent)?.sync_all()?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temp);
    }
    result
}

fn set_owner(file: &File, uid: u32, gid: u32) -> Result<()> {
    // SAFETY: the descriptor is borrowed for the duration of fchown and the
    // integer uid/gid values come directly from stat metadata.
    if unsafe { libc::fchown(file.as_raw_fd(), uid, gid) } == -1 {
        return Err(io::Error::last_os_error()).context("preserve sysprops owner");
    }
    Ok(())
}

fn read_hex_u16(path: &Path) -> Result<u16> {
    let value = fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    u16::from_str_radix(value.trim().trim_start_matches("0x"), 16)
        .with_context(|| format!("parse hexadecimal value in {}", path.display()))
}

fn read_optional_hex_u16(path: &Path) -> Result<Option<u16>> {
    match fs::read_to_string(path) {
        Ok(value) => Ok(Some(
            u16::from_str_radix(value.trim().trim_start_matches("0x"), 16)
                .with_context(|| format!("parse hexadecimal value in {}", path.display()))?,
        )),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error).with_context(|| format!("read {}", path.display())),
    }
}

trait VecDrainFilterRange {
    fn drain_filter_range(
        &mut self,
        start: usize,
        end: usize,
        predicate: impl FnMut(&String) -> bool,
    );
}

impl VecDrainFilterRange for Vec<String> {
    fn drain_filter_range(
        &mut self,
        start: usize,
        end: usize,
        mut predicate: impl FnMut(&String) -> bool,
    ) {
        let mut index = start;
        let mut end = end;
        while index < end {
            if predicate(&self[index]) {
                self.remove(index);
                end -= 1;
            } else {
                index += 1;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    #[test]
    fn resolves_realtek_transport_pair() {
        let identity = UsbControllerIdentity {
            hci: 4,
            vendor: 0x2b89,
            product: 0x8761,
            bcd_device: None,
            sysfs_device: PathBuf::new(),
        };
        let quirk = hfp_transport_quirk(&identity).unwrap();
        assert_eq!(quirk.name, "realtek-rtl8761bu");
        assert_eq!((quirk.msbc_altsetting, quirk.msbc_packet_size), (3, 72));
    }

    #[test]
    fn sysprops_update_is_scoped_and_idempotent() {
        let original = "[Other]\nbluetooth.hfp.linux_hci_driver_msbc_altsetting=6\n\
                        [Sysprops]\nkeep=true\n\
                        bluetooth.hfp.linux_hci_driver_msbc_packet_size=24\n";
        let quirk = HFP_TRANSPORT_QUIRKS[1];
        let updated = render_sysprops_with_quirk(original, quirk);
        assert!(updated.contains("[Other]\nbluetooth.hfp.linux_hci_driver_msbc_altsetting=6"));
        assert!(updated.contains("keep=true"));
        assert!(updated.contains("bluetooth.hfp.linux_hci_driver_msbc_altsetting=3"));
        assert!(updated.contains("bluetooth.hfp.linux_hci_driver_msbc_packet_size=72"));
        assert_eq!(render_sysprops_with_quirk(&updated, quirk), updated);
    }

    #[test]
    fn discovers_usb_identity_through_hci_device_symlink() {
        let fixture = std::env::temp_dir().join(format!(
            "dorsche-controller-test-{}-{}",
            std::process::id(),
            std::thread::current().name().unwrap_or("unnamed")
        ));
        let usb = fixture.join("devices/usb3/3-4");
        let interface = usb.join("3-4:1.0");
        let hci = fixture.join("class/bluetooth/hci4");
        let _ = fs::remove_dir_all(&fixture);
        fs::create_dir_all(&interface).unwrap();
        fs::create_dir_all(&hci).unwrap();
        fs::write(usb.join("idVendor"), "2b89\n").unwrap();
        fs::write(usb.join("idProduct"), "8761\n").unwrap();
        fs::write(usb.join("bcdDevice"), "0200\n").unwrap();
        symlink(&interface, hci.join("device")).unwrap();

        let identity = detect_usb_controller(&fixture.join("class/bluetooth"), 4).unwrap();
        assert_eq!(identity.usb_id(), "2b89:8761");
        assert_eq!(identity.bcd_device, Some(0x0200));
        assert_eq!(identity.sysfs_device, usb.canonicalize().unwrap());

        fs::remove_dir_all(fixture).unwrap();
    }
}
