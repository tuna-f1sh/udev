use std::sync::Arc;

use udevrs::{Result, Udev, UdevHwdb};

mod common;

#[test]
fn parse_hwdb() -> Result<()> {
    common::init();

    std::env::set_var("UDEV_HWDB_BIN", "./hwdb.bin");
    let udev = Arc::new(Udev::new());

    let mut hwdb = UdevHwdb::new(udev)?;
    let exp = ("ID_VENDOR_FROM_DATABASE", "Linux Foundation");

    let found = hwdb
        .get_properties_list_entry("usb:v1D6Bp0001", 0)?
        .find(|e| e.value() == "Linux Foundation")
        .map(|e| (e.name(), e.value()));

    assert_eq!(found, Some(exp));

    Ok(())
}
