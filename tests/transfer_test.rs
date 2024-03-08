use std::{error::Error, fs::read_to_string};

use struson::{
    reader::{json_path::json_path, JsonReader, JsonStreamReader},
    writer::{JsonStreamWriter, JsonWriter, WriterSettings},
};

use crate::test_lib::get_test_data_file_path;

// Ignore dead code warnings because this test does not use all functions from `test_lib`
#[allow(dead_code)]
mod test_lib;

#[futures_test::test]
async fn transfer_test() -> Result<(), Box<dyn Error>> {
    let expected_json = read_to_string(get_test_data_file_path())?;
    // Normalize JSON document string
    let expected_json = expected_json.replace('\r', "");
    let expected_json = expected_json.trim_end();

    let mut json_reader = JsonStreamReader::new(expected_json.as_bytes());

    let mut writer = Vec::<u8>::new();
    let mut json_writer = JsonStreamWriter::new_custom(
        &mut writer,
        WriterSettings {
            pretty_print: true,
            ..Default::default()
        },
    );

    // First wrap and transfer JSON document
    json_writer.begin_object().await?;
    json_writer.name("nested").await?;
    json_writer.begin_array().await?;

    json_reader.transfer_to(&mut json_writer).await?;
    json_reader.consume_trailing_whitespace().await?;

    json_writer.end_array().await?;
    json_writer.end_object().await?;
    json_writer.finish_document().await?;

    let intermediate_json = String::from_utf8(writer)?;

    let mut json_reader = JsonStreamReader::new(intermediate_json.as_bytes());

    let mut writer = Vec::<u8>::new();
    let mut json_writer = JsonStreamWriter::new_custom(
        &mut writer,
        WriterSettings {
            pretty_print: true,
            ..Default::default()
        },
    );

    // Then unwrap it again
    json_reader.seek_to(&json_path!["nested", 0]).await?;
    json_reader.transfer_to(&mut json_writer).await?;
    json_reader.skip_to_top_level().await?;
    json_reader.consume_trailing_whitespace().await?;

    json_writer.finish_document().await?;

    assert_eq!(expected_json, String::from_utf8(writer)?);

    Ok(())
}
