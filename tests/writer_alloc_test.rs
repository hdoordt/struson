use std::error::Error;

use assert_no_alloc::permit_alloc;
// Only use import when creating debug builds, see also configuration below
#[cfg(debug_assertions)]
use assert_no_alloc::AllocDisabler;
use futures::AsyncWriteExt;
use struson::writer::{JsonStreamWriter, JsonWriter, StringValueWriter, WriterSettings};

// Only enable when creating debug builds
#[cfg(debug_assertions)]
#[global_allocator]
static A: AllocDisabler = AllocDisabler;

fn assert_no_alloc<F: FnOnce() -> Result<(), Box<dyn Error>>>(func: F) {
    assert_no_alloc::assert_no_alloc(func).unwrap()
}

fn permit_dealloc<T, F: FnOnce() -> T>(func: F) -> T {
    // TODO: Permitting only dealloc is not possible yet, see https://github.com/Windfisch/rust-assert-no-alloc/issues/15
    permit_alloc(func)
}

fn new_byte_writer() -> Vec<u8> {
    // Pre-allocate to avoid allocations during test execution
    Vec::with_capacity(4096)
}

#[test]
#[ignore = "assert_no_alloc needs async support"]
fn write_values() {
    let mut writer = new_byte_writer();
    let mut json_writer = JsonStreamWriter::new_custom(
        &mut writer,
        WriterSettings {
            // To test creation of surrogate pair escape sequences for supplementary code points
            escape_all_non_ascii: true,
            ..Default::default()
        },
    );

    let large_string = "abcd".repeat(500);

    assert_no_alloc(|| {
        futures::executor::block_on(async {
            json_writer.begin_object().await?;
            json_writer.name("a").await?;

            json_writer.begin_array().await?;
            // Write string which has to be escaped
            json_writer.string_value("\0\n\t \u{10FFFF}").await?;
            json_writer.string_value(&large_string).await?;
            // Note: Cannot use non-string number methods because they perform allocation
            json_writer.number_value_from_string("1234.56e-7").await?;
            json_writer.bool_value(true).await?;
            json_writer.bool_value(false).await?;
            json_writer.null_value().await?;
            json_writer.end_array().await?;

            // Write string which has to be escaped
            json_writer.name("\0\n\t \u{10FFFF}").await?;
            json_writer.bool_value(true).await?;

            json_writer.end_object().await?;

            permit_dealloc(|| json_writer.finish_document()).await?;
            Ok(())
        })
    });

    let expected_json = "{\"a\":[\"\\u0000\\n\\t \\uDBFF\\uDFFF\",\"".to_owned()
        + &large_string
        + "\",1234.56e-7,true,false,null],\"\\u0000\\n\\t \\uDBFF\\uDFFF\":true}";
    assert_eq!(expected_json, String::from_utf8(writer).unwrap());
}

#[test]
#[ignore = "assert_no_alloc needs async support"]
fn pretty_print() {
    let mut writer = new_byte_writer();
    let mut json_writer = JsonStreamWriter::new_custom(
        &mut writer,
        WriterSettings {
            pretty_print: true,
            ..Default::default()
        },
    );

    assert_no_alloc(|| {
        futures::executor::block_on(async {
        json_writer.begin_object().await?;
        json_writer.name("a").await?;
        json_writer.begin_array().await?;

        json_writer.begin_array().await?;
        json_writer.end_array().await?;
        json_writer.begin_object().await?;
        json_writer.end_object().await?;
        json_writer.bool_value(true).await?;

        json_writer.end_array().await?;
        json_writer.end_object().await?;

        permit_dealloc(|| json_writer.finish_document()).await?;
        Ok(())
        })
    });

    let expected_json = "{\n  \"a\": [\n    [],\n    {},\n    true\n  ]\n}";
    assert_eq!(expected_json, String::from_utf8(writer).unwrap());
}

#[test]
#[ignore = "assert_no_alloc needs async support"]
fn string_value_writer() -> Result<(), Box<dyn Error>> {
    let mut writer = new_byte_writer();
    let mut json_writer = JsonStreamWriter::new(&mut writer);
    let large_string = "abcd".repeat(500);

    assert_no_alloc(|| {
        futures::executor::block_on(async {
        let mut string_value_writer = json_writer.string_value_writer().await?;
        string_value_writer.write_all(b"a").await?;
        string_value_writer.write_all(b"\0").await?;
        string_value_writer.write_all(b"\n\t").await?;
        string_value_writer.write_all(large_string.as_bytes()).await?;
        string_value_writer.write_all(b"test").await?;
        string_value_writer.finish_value().await?;

        permit_dealloc(|| json_writer.finish_document()).await?;
        Ok(())
        })
    });

    let expected_json = format!("\"a\\u0000\\n\\t{large_string}test\"");
    assert_eq!(expected_json, String::from_utf8(writer).unwrap());
    Ok(())
}
