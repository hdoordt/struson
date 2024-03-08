//! Integration test for a custom `JsonWriter` implementation
//!
//! The `JsonWriter` implementation here is built on top of serde_json's `Value`.
//! This ensures that the `JsonWriter` trait can be implemented by users, and does
//! not depend on something which is only accessible within Struson.
//!
//! **Important:** This code is only for integration test and demonstration purposes;
//! it is not intended to be used in production code.

use custom_writer::JsonValueWriter;
use futures::AsyncWriteExt;
use serde_json::json;
use struson::{
    reader::{JsonReader, JsonStreamReader},
    writer::{JsonNumberError, JsonWriter, StringValueWriter},
};

mod custom_writer {
    use futures::AsyncWrite;
    use serde_json::{Map, Number, Value};
    use std::{fmt::Display, io::ErrorKind};
    use struson::writer::{
        FiniteNumber, FloatingPointNumber, JsonNumberError, JsonWriter, StringValueWriter,
    };

    type IoError = std::io::Error;

    enum StackValue {
        Array(Vec<Value>),
        Object(Map<String, Value>),
    }

    pub struct JsonValueWriter<'a> {
        stack: Vec<StackValue>,
        pending_name: Option<String>,
        is_string_value_writer_active: bool,
        /// Holds the final value until `finish_document` is called
        final_value_temp: Option<Value>,
        /// Holds the final value after `finish_document` is called, and which is accessible
        /// to creator of `JsonValueWriter` (who should still have a reference to this `Option`)
        final_value_holder: &'a mut Option<Value>,
    }
    impl<'a> JsonValueWriter<'a> {
        /*
         * TODO: This approach of taking an `Option` reference and storing the final value in it
         * matches the current JsonWriter API, however a cleaner approach might be if `finish_document`
         * could return the result instead, see TODO comment on `JsonWriter::finish_document`
         */
        pub fn new(final_value_holder: &'a mut Option<Value>) -> Self {
            if final_value_holder.is_some() {
                panic!("Final value holder should be None");
            }
            JsonValueWriter {
                stack: Vec::new(),
                pending_name: None,
                is_string_value_writer_active: false,
                final_value_temp: None,
                final_value_holder,
            }
        }
    }

    impl JsonValueWriter<'_> {
        fn verify_string_writer_inactive(&self) {
            if self.is_string_value_writer_active {
                panic!("Incorrect writer usage: String value writer is active");
            }
        }

        fn check_before_value(&self) {
            self.verify_string_writer_inactive();
            if self.final_value_temp.is_some() || self.final_value_holder.is_some() {
                panic!("Incorrect writer usage: Top-level value has already been written")
            }
            if let Some(StackValue::Object(_)) = self.stack.last() {
                if self.pending_name.is_none() {
                    panic!("Incorrect writer usage: Member name is expected");
                }
            }
        }

        fn add_value(&mut self, value: Value) {
            if let Some(stack_value) = self.stack.last_mut() {
                match stack_value {
                    StackValue::Array(array) => array.push(value),
                    StackValue::Object(object) => {
                        object.insert(self.pending_name.take().unwrap(), value);
                    }
                };
            } else {
                debug_assert!(
                    self.final_value_temp.is_none(),
                    "caller should have verified that final value is not set yet"
                );
                self.final_value_temp = Some(value);
            }
        }
    }

    fn serde_number_from_f64(f: f64) -> Result<Number, JsonNumberError> {
        Number::from_f64(f)
            .ok_or_else(|| JsonNumberError::InvalidNumber(format!("non-finite number: {f}")))
    }

    impl JsonWriter for JsonValueWriter<'_> {
        async fn begin_object(&mut self) -> Result<(), IoError> {
            self.check_before_value();
            self.stack.push(StackValue::Object(Map::new()));
            Ok(())
        }

        async fn end_object(&mut self) -> Result<(), IoError> {
            self.verify_string_writer_inactive();
            if let Some(StackValue::Object(map)) = self.stack.pop() {
                self.add_value(Value::Object(map));
                Ok(())
            } else {
                panic!("Incorrect writer usage: Cannot end object; not inside object");
            }
        }

        async fn begin_array(&mut self) -> Result<(), IoError> {
            self.check_before_value();
            self.stack.push(StackValue::Array(Vec::new()));
            Ok(())
        }

        async fn end_array(&mut self) -> Result<(), IoError> {
            self.verify_string_writer_inactive();
            if let Some(StackValue::Array(vec)) = self.stack.pop() {
                self.add_value(Value::Array(vec));
                Ok(())
            } else {
                panic!("Incorrect writer usage: Cannot end array; not inside array");
            }
        }

        async fn name(&mut self, name: &str) -> Result<(), IoError> {
            self.verify_string_writer_inactive();
            if let Some(StackValue::Object(_)) = self.stack.last() {
                if self.pending_name.is_some() {
                    panic!("Incorrect writer usage: Member name has already been written; expecting value");
                }
                self.pending_name = Some(name.to_owned());
                Ok(())
            } else {
                panic!("Incorrect writer usage: Cannot write name; not inside object");
            }
        }

        async fn null_value(&mut self) -> Result<(), IoError> {
            self.check_before_value();
            self.add_value(Value::Null);
            Ok(())
        }

        async fn bool_value(&mut self, value: bool) -> Result<(), IoError> {
            self.check_before_value();
            self.add_value(Value::Bool(value));
            Ok(())
        }

        async fn string_value(&mut self, value: &str) -> Result<(), IoError> {
            self.check_before_value();
            self.add_value(Value::String(value.to_owned()));
            Ok(())
        }

        async fn string_value_writer(
            &mut self,
        ) -> Result<impl StringValueWriter + Unpin + '_, IoError> {
            self.check_before_value();
            self.is_string_value_writer_active = true;
            Ok(StringValueWriterImpl {
                buf: Vec::new(),
                json_writer: self,
            })
        }

        async fn number_value_from_string(&mut self, value: &str) -> Result<(), JsonNumberError> {
            self.check_before_value();
            // TODO: `parse::<f64>` might not match JSON number string format (might allow more / less than allowed by JSON)?
            let f = value
                .parse::<f64>()
                .map_err(|e| JsonNumberError::InvalidNumber(e.to_string()))?;
            self.add_value(Value::Number(serde_number_from_f64(f)?));
            Ok(())
        }

        async fn number_value<N: FiniteNumber>(&mut self, value: N) -> Result<(), IoError> {
            let number = value
                .as_u64()
                .map(Number::from)
                .or_else(|| value.as_i64().map(Number::from));

            if let Some(n) = number {
                self.check_before_value();
                self.add_value(Value::Number(n));
                Ok(())
            } else {
                let number_str = value.to_string();

                self.number_value_from_string(&number_str)
                    .await
                    .map_err(|e| match e {
                        JsonNumberError::InvalidNumber(e) => {
                            panic!("Unexpected: Writer rejected finite number '{number_str}': {e}")
                        }
                        JsonNumberError::IoError(e) => IoError::new(ErrorKind::Other, e),
                    })
            }
        }

        async fn fp_number_value<N: FloatingPointNumber>(
            &mut self,
            value: N,
        ) -> Result<(), JsonNumberError> {
            let number = if let Some(n) = value.as_f64() {
                Some(serde_number_from_f64(n)?)
            } else {
                None
            };

            if let Some(n) = number {
                self.check_before_value();
                self.add_value(Value::Number(n));
                Ok(())
            } else {
                // TODO: Cannot match over possible implementations? Therefore have to use string representation
                let number_str = value.to_string();

                self.number_value_from_string(&number_str)
                    .await
                    .map_err(|e| {
                        match e {
                            // `use_json_number` should have verified that value is valid finite JSON number
                            JsonNumberError::InvalidNumber(e) => {
                                panic!(
                                    "Unexpected: Writer rejected finite number '{number_str}': {e}"
                                )
                            }
                            JsonNumberError::IoError(e) => IoError::new(ErrorKind::Other, e),
                        }
                    })?;
                Ok(())
            }
        }

        #[cfg(feature = "serde")]
        async fn serialize_value<S: serde::Serialize>(
            &mut self,
            value: &S,
        ) -> Result<(), struson::serde::SerializerError> {
            self.check_before_value();
            let mut serializer = struson::serde::JsonWriterSerializer::new(self);
            value.serialize(&mut serializer)
            // TODO: Verify that value was properly serialized (only single value; no incomplete array or object)
            // might not be necessary because Serde's Serialize API enforces this
        }

        async fn finish_document(mut self) -> Result<(), IoError> {
            self.verify_string_writer_inactive();
            if let Some(value) = self.final_value_temp.take() {
                *self.final_value_holder = Some(value);
                Ok(())
            } else {
                panic!("Incorrect writer usage: Top-level value is incomplete")
            }
        }
    }

    struct StringValueWriterImpl<'j, 'a> {
        buf: Vec<u8>,
        json_writer: &'j mut JsonValueWriter<'a>,
    }
    impl AsyncWrite for StringValueWriterImpl<'_, '_> {
        fn poll_write(
            mut self: std::pin::Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
            buf: &[u8],
        ) -> std::task::Poll<std::io::Result<usize>> {
            self.buf.extend_from_slice(buf);
            std::task::Poll::Ready(Ok(buf.len()))
        }

        fn poll_flush(
            self: std::pin::Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<std::io::Result<()>> {
            std::task::Poll::Ready(Ok(()))
        }

        fn poll_close(
            self: std::pin::Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<std::io::Result<()>> {
            std::task::Poll::Ready(Ok(()))
        }
    }
    impl StringValueWriter for StringValueWriterImpl<'_, '_> {
        async fn finish_value(self) -> Result<(), IoError> {
            let string =
                String::from_utf8(self.buf).map_err(|e| IoError::new(ErrorKind::InvalidData, e))?;
            self.json_writer.add_value(Value::String(string));
            self.json_writer.is_string_value_writer_active = false;
            Ok(())
        }
    }
}

#[futures_test::test]
async fn write() -> Result<(), Box<dyn std::error::Error>> {
    fn assert_invalid_number(expected_message: Option<&str>, result: Result<(), JsonNumberError>) {
        match result {
            Err(JsonNumberError::InvalidNumber(message)) => {
                if let Some(expected_message) = expected_message {
                    assert_eq!(expected_message, message)
                }
            }
            _ => panic!("Unexpected result: {result:?}"),
        }
    }

    let mut final_value_holder = None;
    let mut json_writer = JsonValueWriter::new(&mut final_value_holder);

    json_writer.begin_array().await?;

    json_writer.begin_object().await?;
    json_writer.name("name1").await?;
    json_writer.begin_array().await?;
    json_writer.bool_value(true).await?;
    json_writer.end_array().await?;
    json_writer.name("name2").await?;
    json_writer.bool_value(false).await?;
    json_writer.end_object().await?;

    json_writer.null_value().await?;
    json_writer.bool_value(true).await?;
    json_writer.bool_value(false).await?;
    json_writer.string_value("string").await?;

    let mut string_writer = json_writer.string_value_writer().await?;
    string_writer.write_all("first ".as_bytes()).await?;
    string_writer.write_all("second".as_bytes()).await?;
    string_writer.finish_value().await?;

    json_writer.number_value_from_string("123").await?;
    assert_invalid_number(
        Some(&format!("non-finite number: {}", f64::INFINITY)),
        json_writer.number_value_from_string("Infinity").await,
    );
    // Don't check for exact error message because it is created by Rust and might change in the future
    assert_invalid_number(None, json_writer.number_value_from_string("test").await);
    json_writer.number_value(45).await?;
    json_writer.number_value(-67).await?;
    json_writer.fp_number_value(8.9).await?;
    assert_invalid_number(
        Some(&format!("non-finite number: {}", f64::INFINITY)),
        json_writer.fp_number_value(f64::INFINITY).await,
    );

    json_writer.end_array().await?;
    json_writer.finish_document().await?;

    let expected_json = json!([
        {
            "name1": [true],
            "name2": false,
        },
        null,
        true,
        false,
        "string",
        "first second",
        // Current number from string implementation always writes f64
        123.0,
        45,
        -67,
        8.9,
    ]);
    assert_eq!(expected_json, final_value_holder.unwrap());

    Ok(())
}

#[futures_test::test]
async fn transfer() -> Result<(), Box<dyn std::error::Error>> {
    let mut final_value_holder = None;
    let mut json_writer = JsonValueWriter::new(&mut final_value_holder);

    let mut json_reader = JsonStreamReader::new(
        "[true, 123, {\"name1\": \"value1\", \"name2\": null}, false]".as_bytes(),
    );
    json_reader.transfer_to(&mut json_writer).await?;
    json_reader.consume_trailing_whitespace().await?;

    json_writer.finish_document().await?;

    let expected_json = json!([
        true,
        // Current number from string implementation always writes f64
        123.0,
        {
            "name1": "value1",
            "name2": null,
        },
        false,
    ]);
    assert_eq!(expected_json, final_value_holder.unwrap());

    Ok(())
}

#[cfg(feature = "serde")]
#[futures_test::test]
async fn serialize() -> Result<(), Box<dyn std::error::Error>> {
    use serde::Serialize;

    #[derive(Serialize)]
    struct CustomStruct {
        a: u32,
        b: &'static str,
    }

    let mut final_value_holder = None;
    let mut json_writer = JsonValueWriter::new(&mut final_value_holder);
    json_writer.serialize_value(&CustomStruct { a: 123, b: "test" })?;
    json_writer.finish_document()?;

    let expected_json = json!({
        "a": 123,
        "b": "test",
    });
    assert_eq!(expected_json, final_value_holder.unwrap());

    Ok(())
}
