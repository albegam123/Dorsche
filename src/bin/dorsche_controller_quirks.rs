use std::path::PathBuf;

use anyhow::{Result, bail};
use dorsche::wireless::controller::{
    apply_hfp_transport_quirk, detect_usb_controller, hfp_transport_quirk,
};

#[derive(Debug)]
struct Args {
    hci: u32,
    sysfs_root: PathBuf,
    sysprops: PathBuf,
    apply: bool,
    strict: bool,
}

impl Args {
    fn parse() -> Result<Self> {
        let mut parsed = Self {
            hci: 0,
            sysfs_root: PathBuf::from("/sys/class/bluetooth"),
            sysprops: PathBuf::from("/var/lib/bluetooth/sysprops.conf"),
            apply: false,
            strict: false,
        };
        let mut args = std::env::args().skip(1);
        while let Some(arg) = args.next() {
            match arg.as_str() {
                "--hci" => {
                    parsed.hci = args
                        .next()
                        .ok_or_else(|| anyhow::anyhow!("--hci requires an index"))?
                        .parse()?;
                }
                "--sysfs-root" => {
                    parsed.sysfs_root = PathBuf::from(
                        args.next()
                            .ok_or_else(|| anyhow::anyhow!("--sysfs-root requires a path"))?,
                    );
                }
                "--sysprops" => {
                    parsed.sysprops = PathBuf::from(
                        args.next()
                            .ok_or_else(|| anyhow::anyhow!("--sysprops requires a path"))?,
                    );
                }
                "--apply" => parsed.apply = true,
                "--strict" => parsed.strict = true,
                "-h" | "--help" => {
                    println!(
                        "Usage: dorsche_controller_quirks [--hci 0] [--apply] [--strict] \
                         [--sysfs-root /sys/class/bluetooth] \
                         [--sysprops /var/lib/bluetooth/sysprops.conf]"
                    );
                    std::process::exit(0);
                }
                _ => bail!("unknown argument: {arg}"),
            }
        }
        Ok(parsed)
    }
}

fn main() -> Result<()> {
    let args = Args::parse()?;
    let identity = match detect_usb_controller(&args.sysfs_root, args.hci) {
        Ok(identity) => identity,
        Err(error) if !args.strict => {
            eprintln!(
                "controller=hci{} action=unchanged reason={error:#}",
                args.hci
            );
            return Ok(());
        }
        Err(error) => return Err(error),
    };
    println!(
        "controller=hci{} usb={} bcd_device={} sysfs={}",
        identity.hci,
        identity.usb_id(),
        identity
            .bcd_device
            .map_or_else(|| "unknown".to_owned(), |value| format!("{value:04x}")),
        identity.sysfs_device.display()
    );
    let Some(quirk) = hfp_transport_quirk(&identity) else {
        println!("hfp_transport_quirk=none action=unchanged");
        return Ok(());
    };
    println!(
        "hfp_transport_quirk={} msbc_altsetting={} msbc_packet_size={}",
        quirk.name, quirk.msbc_altsetting, quirk.msbc_packet_size
    );
    if args.apply {
        let changed = apply_hfp_transport_quirk(&args.sysprops, quirk)?;
        println!(
            "sysprops={} action={}",
            args.sysprops.display(),
            if changed { "updated" } else { "unchanged" }
        );
    } else {
        println!("action=dry-run");
    }
    Ok(())
}
