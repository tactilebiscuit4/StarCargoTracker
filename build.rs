//! Gives the Windows exe its icon (assets/icon/star-cargo-tracker.ico) and
//! version details. Other platforms have nothing to embed.

fn main() {
    println!("cargo:rerun-if-changed=assets/icon/star-cargo-tracker.ico");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }
    let mut resource = winresource::WindowsResource::new();
    resource
        .set_icon("assets/icon/star-cargo-tracker.ico")
        .set("FileDescription", "Star Cargo Tracker")
        .set("ProductName", "Star Cargo Tracker");
    if let Err(e) = resource.compile() {
        // Building without the icon beats not building at all.
        println!("cargo:warning=Couldn't embed the icon: {e}");
    }
}
