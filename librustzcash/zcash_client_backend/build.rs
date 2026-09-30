#[cfg(not(windows))]
use std::env;
#[cfg(not(windows))]
use std::fs;
use std::io;
#[cfg(not(windows))]
use std::path::{Path, PathBuf};

// The proto machinery below is only ever exercised on non-Windows targets (see the note in
// `main`), so gate it on `not(windows)` to avoid dead-code and unused-import warnings on Windows.
#[cfg(not(windows))]
const COMPACT_FORMATS_PROTO: &str = "proto/compact_formats.proto";

#[cfg(not(windows))]
const PROPOSAL_PROTO: &str = "proto/proposal.proto";

#[cfg(not(windows))]
const SERVICE_PROTO: &str = "proto/service.proto";

fn main() -> io::Result<()> {
    // Ordinary builds always consume the checked-in bindings. Generation is
    // explicit, including on machines where protoc happens to be installed.
    println!("cargo:rerun-if-env-changed=ZAKURA_PROTO_MODE");
    println!("cargo:rerun-if-env-changed=PROTOC");
    #[cfg(not(windows))]
    for path in [COMPACT_FORMATS_PROTO, PROPOSAL_PROTO, SERVICE_PROTO] {
        println!("cargo:rerun-if-changed={path}");
    }
    for path in [
        "src/proto/compact_formats.rs",
        "src/proto/proposal.rs",
        "src/proto/service.rs",
    ] {
        println!("cargo:rerun-if-changed={path}");
    }
    if let Some(mode) = std::env::var_os("ZAKURA_PROTO_MODE") {
        #[cfg(windows)]
        return Err(io::Error::other("regenerate protobufs on macOS or Linux"));
        #[cfg(not(windows))]
        {
            let mode = mode
                .to_str()
                .ok_or_else(|| io::Error::other("invalid protobuf mode"))?;
            if !matches!(mode, "check" | "write") {
                return Err(io::Error::other("ZAKURA_PROTO_MODE must be check or write"));
            }
            if !Path::new(COMPACT_FORMATS_PROTO).exists() {
                return Err(io::Error::other(
                    "protobuf sources are absent; use the repository checkout",
                ));
            }
            build(mode)?;
        }
    }

    Ok(())
}

#[cfg(not(windows))]
fn build(mode: &str) -> io::Result<()> {
    let out: PathBuf = env::var_os("OUT_DIR")
        .expect("Cannot find OUT_DIR environment variable")
        .into();

    // Build the compact format types.
    tonic_prost_build::compile_protos(COMPACT_FORMATS_PROTO)?;

    // Copy the generated types into the source tree so changes can be committed.
    install_binding(
        &out.join("cash.z.wallet.sdk.rpc.rs"),
        "src/proto/compact_formats.rs",
        mode,
    )?;

    // Build the gRPC types and client.
    tonic_prost_build::configure()
        .build_server(false)
        .client_mod_attribute(
            "cash.z.wallet.sdk.rpc",
            r#"#[cfg(feature = "lightwalletd-tonic")]"#,
        )
        .extern_path(
            ".cash.z.wallet.sdk.rpc.ChainMetadata",
            "crate::proto::compact_formats::ChainMetadata",
        )
        .extern_path(
            ".cash.z.wallet.sdk.rpc.CompactBlock",
            "crate::proto::compact_formats::CompactBlock",
        )
        .extern_path(
            ".cash.z.wallet.sdk.rpc.CompactTx",
            "crate::proto::compact_formats::CompactTx",
        )
        .extern_path(
            ".cash.z.wallet.sdk.rpc.CompactTxIn",
            "crate::proto::compact_formats::CompactTxIn",
        )
        .extern_path(
            ".cash.z.wallet.sdk.rpc.TxOut",
            "crate::proto::compact_formats::TxOut",
        )
        .extern_path(
            ".cash.z.wallet.sdk.rpc.CompactSaplingSpend",
            "crate::proto::compact_formats::CompactSaplingSpend",
        )
        .extern_path(
            ".cash.z.wallet.sdk.rpc.CompactSaplingOutput",
            "crate::proto::compact_formats::CompactSaplingOutput",
        )
        .extern_path(
            ".cash.z.wallet.sdk.rpc.CompactOrchardAction",
            "crate::proto::compact_formats::CompactOrchardAction",
        )
        .extern_path(
            ".cash.z.wallet.sdk.rpc.OutPoint",
            "crate::proto::compact_formats::OutPoint",
        )
        .compile_protos(&[SERVICE_PROTO], &["proto/"])?;

    // Build the proposal types.
    tonic_prost_build::compile_protos(PROPOSAL_PROTO)?;

    // Copy the generated types into the source tree so changes can be committed.
    install_binding(
        &out.join("cash.z.wallet.sdk.ffi.rs"),
        "src/proto/proposal.rs",
        mode,
    )?;

    // Copy the generated types into the source tree so changes can be committed. The
    // file has the same name as for the compact format types because they have the
    // same package, but we've set things up so this only contains the service types.
    install_binding(
        &out.join("cash.z.wallet.sdk.rpc.rs"),
        "src/proto/service.rs",
        mode,
    )?;

    Ok(())
}

#[cfg(not(windows))]
fn install_binding(generated: &Path, checked_in: &str, mode: &str) -> io::Result<()> {
    if mode == "write" {
        fs::copy(generated, checked_in)?;
    } else if fs::read(generated)? != fs::read(checked_in)? {
        return Err(io::Error::other(format!(
            "{checked_in} is stale; run python3 scripts/proto.py write"
        )));
    }
    Ok(())
}
