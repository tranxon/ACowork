fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Use vendored protoc so contributors and CI don't need to install protoc manually.
    // This works on Windows, macOS, and Linux without any extra setup.
    let protoc_path = protoc_bin_vendored::protoc_bin_path()
        .expect("protoc-bin-vendored: failed to locate bundled protoc binary");
    // SAFETY: build scripts are single-threaded; setting PROTOC here is safe.
    unsafe { std::env::set_var("PROTOC", protoc_path) };

    // build_transport(false) prevents generating the static `connect()` method
    // on GatewayServiceClient<Channel>, which would conflict with the
    // generated RPC method named `Connect`.
    tonic_build::configure()
        .build_transport(false)
        .compile_protos(&["proto/gateway_ipc.proto"], &["proto"])?;
    Ok(())
}
