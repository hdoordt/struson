use std::{error::Error, fmt::Debug, fs::File};

use struson::reader::{JsonReader, JsonStreamReader, ValueType};

use crate::test_lib::{get_expected_events, get_test_data_file_path, JsonEvent};

mod test_lib;

/// Assertion slices for slices which provides more useful error messages than `assert_eq!`
fn assert_slice_eq<T: PartialEq + Debug>(left: &[T], right: &[T]) {
    let iter_len = left.len().min(right.len());

    for i in 0..iter_len {
        assert_eq!(left[i], right[i], "Elements at index {i} don't match");
    }

    // Only check length mismatch afterwards, to detect mismatching items (if any) first
    assert_eq!(left.len(), right.len(), "Slices have different lengths");
}

#[futures_test::test]
async fn reader_test() -> Result<(), Box<dyn Error>> {
    let test_data = std::fs::read_to_string(get_test_data_file_path())?;
    let mut json_reader = JsonStreamReader::new(test_data.as_bytes());
    let mut events = Vec::new();

    enum StackValue {
        Array,
        Object,
    }

    let mut stack = Vec::new();
    loop {
        if !stack.is_empty() {
            match stack.last().unwrap() {
                StackValue::Array => {
                    if !json_reader.has_next().await? {
                        stack.pop();
                        json_reader.end_array().await?;
                        events.push(JsonEvent::ArrayEnd);

                        if stack.is_empty() {
                            break;
                        } else {
                            continue;
                        }
                    }
                }
                StackValue::Object => {
                    if json_reader.has_next().await? {
                        events.push(JsonEvent::MemberName(json_reader.next_name_owned().await?));
                        // fall through to value reading
                    } else {
                        stack.pop();
                        json_reader.end_object().await?;
                        events.push(JsonEvent::ObjectEnd);

                        if stack.is_empty() {
                            break;
                        } else {
                            continue;
                        }
                    }
                }
            }
        }

        match json_reader.peek().await? {
            ValueType::Array => {
                json_reader.begin_array().await?;
                stack.push(StackValue::Array);
                events.push(JsonEvent::ArrayStart);
            }
            ValueType::Object => {
                json_reader.begin_object().await?;
                stack.push(StackValue::Object);
                events.push(JsonEvent::ObjectStart);
            }
            ValueType::String => {
                events.push(JsonEvent::StringValue(json_reader.next_string().await?));
            }
            ValueType::Number => {
                events.push(JsonEvent::NumberValue(
                    json_reader.next_number_as_string().await?,
                ));
            }
            ValueType::Boolean => {
                events.push(JsonEvent::BoolValue(json_reader.next_bool().await?));
            }
            ValueType::Null => {
                json_reader.next_null().await?;
                events.push(JsonEvent::NullValue);
            }
        }

        if stack.is_empty() {
            break;
        }
    }
    json_reader.consume_trailing_whitespace().await?;

    assert_slice_eq(&get_expected_events(), &events);

    Ok(())
}
