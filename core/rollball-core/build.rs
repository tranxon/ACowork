fn main() -> Result<(), Box<dyn std::error::Error>> {
    // build_transport(false) prevents generating the static `connect()` method
    // on GatewayServiceClient<Channel>, which would conflict with the
    // generated RPC method named `Connect`.
    let proto_files: &[&str] = &["proto/gateway_ipc.proto"];
    let includes: &[&str] = &["proto"];
    tonic_build::configure()
        .build_transport(false)
        .compile_protos(proto_files, includes)?;
    Ok(())
}
