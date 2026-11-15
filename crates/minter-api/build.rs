use prost::Message;
use prost_types::FileDescriptorSet;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut schema = FileDescriptorSet::decode(include_bytes!("schema/minter-v2.pb").as_slice())?;
    println!("cargo:rerun-if-changed=schema/minter-v2.pb");
    // The Go schema's "_" enum value has no Rust identifier after case conversion.
    // Keep the original descriptor for ProtoJSON and alias it only for Rust codegen.
    let transaction = schema
        .file
        .iter_mut()
        .filter(|file| file.package() == "api_pb")
        .flat_map(|file| &mut file.message_type)
        .find(|message| message.name() == "TransactionResponse")
        .expect("Minter transaction descriptor");
    let zero = transaction
        .enum_type
        .iter_mut()
        .find(|enumeration| enumeration.name() == "Type")
        .and_then(|enumeration| {
            enumeration
                .value
                .iter_mut()
                .find(|value| value.name() == "_")
        })
        .expect("Minter transaction type zero");
    zero.name = Some("Unspecified".into());
    let mut config = prost_build::Config::new();
    config.enable_type_names();
    tonic_prost_build::configure()
        .generate_default_stubs(true)
        .compile_fds_with_config(schema, config)?;
    Ok(())
}
