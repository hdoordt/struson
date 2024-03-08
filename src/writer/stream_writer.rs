//! Streaming implementation of [`JsonWriter`]

use std::{fmt::Display, io::ErrorKind, str::Utf8Error};

use futures::Future;

use super::*;
use crate::utf8;

/// Settings to customize the JSON writer behavior
///
/// Except for [allowing multiple top-level values](WriterSettings::multi_top_level_value_separator) these
/// settings only have an effect on how the JSON output will look like without affecting
/// its data in any way. All compliant JSON readers should consider the data identical.
///
/// These settings are used by [`JsonStreamWriter::new_custom`]. To avoid repeating the
/// default values for unchanged settings `..Default::default()` can be used:
/// ```
/// # use struson::writer::WriterSettings;
/// WriterSettings {
///     pretty_print: true,
///     // For all other settings use the default
///     ..Default::default()
/// }
/// # ;
/// ```
#[derive(Clone, Debug)]
pub struct WriterSettings {
    /// Whether to pretty print the JSON output
    ///
    /// When enabled the JSON output will have spaces and line breaks to make it easier
    /// for humans to read. Otherwise the JSON output will be compact and have no whitespace.
    /// Pretty printed JSON output might for example look like this:
    /// ```json
    /// {
    ///     "a": [
    ///         1,
    ///         2
    ///     ]
    /// }
    /// ```
    /// Whereas compact JSON output would look like this:
    /// ```json
    /// {"a":[1,2]}
    /// ```
    ///
    /// This setting does not have any effect on the validity of the JSON output.
    /// Pretty printed JSON is allowed by the JSON specification.
    pub pretty_print: bool,

    /// Whether to escape all control characters
    ///
    /// The JSON specification only requires that the Unicode control characters `0x00` to `0x1F`
    /// (inclusive) must be escaped in member names and string values. When this setting
    /// is enabled additionally all Unicode characters for which [`char::is_control`] returns
    /// true will be escaped.
    ///
    /// This setting does not have any effect on the validity of the JSON output. Any
    /// character in member names and string values may be written as escape sequence.
    pub escape_all_control_chars: bool,

    /// Whether to escape all non-ASCII characters
    ///
    /// When enabled all Unicode characters in member names and string values whose code point
    /// is >= `0x80` are written as escape sequence. This can be useful when interacting with
    /// legacy systems which do not properly support non-ASCII input.
    ///
    /// This setting does not have any effect on the validity of the JSON output. Any
    /// character in member names and string values may be written as escape sequence.
    pub escape_all_non_ascii: bool,

    /// Whether to allow multiple top-level values, and if allowed which separator to use
    ///
    /// When `None` multiple top-level values are not allowed. Otherwise when `Some(...)` it
    /// specifies the separator to use between multiple top-level values. The separator can
    /// be an arbitrary string, however there are a few things to keep in mind:
    /// - An empty string (`""`) might prevent some JSON values from being properly parsed.
    ///   For example the values `true` and `false` would be written as `truefalse` which
    ///   might not be accepted as valid JSON by some JSON reader implementations.
    /// - Using something different than regular JSON whitespace (space, `\t`, `\r` and `\n`)
    ///   might lead to output which cannot be parsed properly by some JSON reader implementations.
    ///
    /// For example, with the separator `" ### "` writing the values `123`, `true` and `[]`
    /// would yield: `123 ### true ### []`
    ///
    /// Normally a JSON document is expected to contain only a single top-level value, but there
    /// might be use cases where supporting multiple top-level values can be useful.
    pub multi_top_level_value_separator: Option<String>,
}

impl Default for WriterSettings {
    /// Creates the default JSON writer settings
    ///
    /// - pretty print: disabled (= compact JSON will be written)
    /// - escape all control chars: false (= only control characters `0x00` to `0x1F` are escaped)
    /// - multiple top-level values: disallowed
    fn default() -> Self {
        WriterSettings {
            pretty_print: false,
            escape_all_control_chars: false,
            escape_all_non_ascii: false,
            multi_top_level_value_separator: None,
        }
    }
}

#[derive(PartialEq, Debug)]
enum StackValue {
    Array,
    Object,
}

const WRITER_BUF_SIZE: usize = 1024;

/// A JSON writer implementation which writes data to a [`Write`]
///
/// This writer internally buffers data so it is normally not necessary to wrap the provided
/// writer in a [`std::io::BufWriter`].
///
/// The data written to the underlying writer will be valid UTF-8 data if the JSON document
/// is finished properly by calling [`JsonWriter::finish_document`]. No leading byte order mark (BOM)
/// is written.
///
/// If the underlying writer returns an error of kind [`ErrorKind::Interrupted`], this
/// JSON writer will keep retrying to write the data.
pub struct JsonStreamWriter<W: AsyncWrite> {
    // When adding more fields to this struct, adjust the Debug implementation below, if necessary
    writer: W,
    buf: [u8; WRITER_BUF_SIZE],
    /// Index (starting at 0) within [`buf`](Self::buf) where to write next,
    /// respectively how many bytes have already been written to the buffer
    buf_write_pos: usize,
    /// Whether the current array or object is empty, or at top-level whether
    /// at least one value has been written already
    is_empty: bool,
    expects_member_name: bool,
    stack: Vec<StackValue>,
    is_string_value_writer_active: bool,
    indentation_level: u32,

    writer_settings: WriterSettings,
}

// Implementation with public constructor methods
impl<W: AsyncWrite> JsonStreamWriter<W> {
    /// Creates a JSON writer with [default settings](WriterSettings::default)
    pub fn new(writer: W) -> Self {
        JsonStreamWriter::new_custom(writer, WriterSettings::default())
    }

    /// Creates a JSON writer with custom settings
    ///
    /// The settings can be used to customize how the JSON output will look like.
    pub fn new_custom(writer: W, writer_settings: WriterSettings) -> Self {
        Self {
            writer,
            buf: [0; WRITER_BUF_SIZE],
            buf_write_pos: 0,
            is_empty: true,
            expects_member_name: false,
            stack: Vec::with_capacity(16),
            is_string_value_writer_active: false,
            indentation_level: 0,
            writer_settings,
        }
    }
}

// Implementation with low level byte writing methods
impl<W: AsyncWrite + Unpin> JsonStreamWriter<W> {
    async fn write_bytes(&mut self, bytes: &[u8]) -> Result<(), IoError> {
        let mut pos = 0;
        while pos < bytes.len() {
            let copied_count = (self.buf.len() - self.buf_write_pos).min(bytes.len() - pos);
            self.buf[self.buf_write_pos..(self.buf_write_pos + copied_count)]
                .copy_from_slice(&bytes[pos..(pos + copied_count)]);
            self.buf_write_pos += copied_count;
            pos += copied_count;

            if self.buf_write_pos >= self.buf.len() {
                // write_all retries on `ErrorKind::Interrupted`, as desired
                self.writer.write_all(&self.buf).await?;
                self.buf_write_pos = 0;
            }
        }

        Ok(())
    }

    async fn flush(&mut self) -> Result<(), IoError> {
        // write_all retries on `ErrorKind::Interrupted`, as desired
        self.writer
            .write_all(&self.buf[0..self.buf_write_pos])
            .await?;
        self.buf_write_pos = 0;
        self.writer.flush().await
    }
}

// Implementation with JSON structure state inspection methods, and general value methods
impl<W: AsyncWrite + Unpin> JsonStreamWriter<W> {
    fn is_in_array(&self) -> bool {
        self.stack.last().map_or(false, |v| v == &StackValue::Array)
    }

    fn is_in_object(&self) -> bool {
        self.stack
            .last()
            .map_or(false, |v| v == &StackValue::Object)
    }

    fn increase_indentation(&mut self) {
        self.indentation_level += 1;
    }

    fn decrease_indentation(&mut self) {
        self.indentation_level -= 1;
    }

    async fn write_indentation(&mut self) -> Result<(), IoError> {
        for _ in 0..self.indentation_level {
            self.write_bytes(b"  ").await?;
        }
        Ok(())
    }

    async fn before_container_element(&mut self) -> Result<(), IoError> {
        if self.is_empty {
            if self.writer_settings.pretty_print {
                // Convert "[" (respectively "{") to "[\n..."
                self.write_bytes(b"\n").await?;
                self.increase_indentation();
                self.write_indentation().await?;
            }
        } else {
            #[allow(clippy::collapsible_else_if)]
            if self.writer_settings.pretty_print {
                self.write_bytes(b",\n").await?;
                self.write_indentation().await?;
            } else {
                self.write_bytes(b",").await?;
            }
        }
        Ok(())
    }

    async fn before_value(&mut self) -> Result<(), IoError> {
        if self.is_string_value_writer_active {
            panic!("Incorrect writer usage: Cannot finish document when string value writer is still active");
        }
        if self.expects_member_name {
            panic!("Incorrect writer usage: Cannot write value when name is expected");
        }

        let is_behind_top_level = !self.is_empty && self.stack.is_empty();
        if is_behind_top_level {
            match &self.writer_settings.multi_top_level_value_separator {
                None => panic!("Incorrect writer usage: Cannot write multiple top-level values when not enabled in writer settings"),
                Some(separator) => {
                    // TODO: Avoid clone() here; compiler currently does not allow borrowing it because
                    //   `write_bytes` has a mutable borrow to self
                    let separator = separator.clone();
                    self.write_bytes(separator.as_bytes()).await?;
                },
            }
        } else if self.is_in_array() {
            self.before_container_element().await?;
        }
        self.is_empty = false;

        if self.is_in_object() {
            // After this value a name will be expected
            self.expects_member_name = true;
        }

        Ok(())
    }

    async fn on_container_end(&mut self) -> Result<(), IoError> {
        self.stack.pop();

        if !self.is_empty && self.writer_settings.pretty_print {
            self.write_bytes(b"\n").await?;
            self.decrease_indentation();
            self.write_indentation().await?;
        }

        // Enclosing container is not empty since this method call here is processing its child
        self.is_empty = false;

        // If after pop() call above currently in object, then expecting a member name
        self.expects_member_name = self.is_in_object();
        Ok(())
    }
}

// Implementation with string writing methods
impl<W: AsyncWrite + Unpin> JsonStreamWriter<W> {
    fn should_escape(&self, c: char) -> bool {
        c == '"' || c == '\\'
        // Control characters which must be escaped per JSON specification
        || ('\u{0}'..='\u{1F}').contains(&c)
            || (self.writer_settings.escape_all_non_ascii && !c.is_ascii())
            || (self.writer_settings.escape_all_control_chars && c.is_control())
    }

    async fn write_escaped_char(&mut self, c: char) -> Result<(), IoError> {
        fn get_unicode_escape(value: u32) -> [u8; 4] {
            // For convenience `value` is u32, but it is actually u16
            debug_assert!(value <= u16::MAX as u32);

            fn to_hex(i: u32) -> u8 {
                match i {
                    0..=9 => b'0' + i as u8,
                    10..=15 => b'A' + (i - 10) as u8,
                    _ => unreachable!("Unexpected value {i}"),
                }
            }

            [
                to_hex(value >> 12 & 15),
                to_hex(value >> 8 & 15),
                to_hex(value >> 4 & 15),
                to_hex(value & 15),
            ]
        }

        let escape = match c {
            '"' => "\\\"",
            '\\' => "\\\\",
            '/' => "\\/",
            '\u{0008}' => "\\b",
            '\u{000C}' => "\\f",
            '\n' => "\\n",
            '\r' => "\\r",
            '\t' => "\\t",
            '\0'..='\u{FFFF}' => {
                self.write_bytes(b"\\u").await?;
                self.write_bytes(&get_unicode_escape(c as u32)).await?;
                return Ok(());
            }
            _ => {
                // Encode as surrogate pair
                let temp = (c as u32) - 0x10000;
                let high = (temp >> 10) + 0xD800;
                let low = (temp & ((1 << 10) - 1)) + 0xDC00;

                self.write_bytes(b"\\u").await?;
                self.write_bytes(&get_unicode_escape(high)).await?;

                self.write_bytes(b"\\u").await?;
                self.write_bytes(&get_unicode_escape(low)).await?;
                return Ok(());
            }
        };
        self.write_bytes(escape.as_bytes()).await
    }

    async fn write_string_value_piece(&mut self, value: &str) -> Result<(), IoError> {
        let bytes = value.as_bytes();
        let mut next_to_write_index = 0;

        for (index, char) in value.char_indices() {
            if self.should_escape(char) {
                if index > next_to_write_index {
                    self.write_bytes(&bytes[next_to_write_index..index]).await?;
                }
                self.write_escaped_char(char).await?;
                next_to_write_index = index + char.len_utf8();
            }
        }
        // Write remaining bytes
        if next_to_write_index < bytes.len() {
            self.write_bytes(&bytes[next_to_write_index..]).await?;
        }

        Ok(())
    }

    async fn write_string_value(&mut self, value: &str) -> Result<(), IoError> {
        self.write_bytes(b"\"").await?;
        self.write_string_value_piece(value).await?;
        self.write_bytes(b"\"").await
    }
}

impl<W: AsyncWrite + Unpin> JsonWriter for JsonStreamWriter<W> {
    async fn begin_object(&mut self) -> Result<(), IoError> {
        self.before_value().await?;
        self.stack.push(StackValue::Object);
        self.is_empty = true;
        self.expects_member_name = true;
        self.write_bytes(b"{").await
    }

    async fn name(&mut self, name: &str) -> Result<(), IoError> {
        if !self.expects_member_name {
            panic!("Incorrect writer usage: Cannot write name when name is not expected");
        }
        if self.is_string_value_writer_active {
            panic!("Incorrect writer usage: Cannot finish document when string value writer is still active");
        }
        self.before_container_element().await?;
        self.write_string_value(name).await?;
        self.write_bytes(if self.writer_settings.pretty_print {
            b": "
        } else {
            b":"
        })
        .await?;
        self.expects_member_name = false;

        Ok(())
    }

    async fn end_object(&mut self) -> Result<(), IoError> {
        if !self.is_in_object() {
            panic!("Incorrect writer usage: Cannot end object when not inside object");
        }
        if self.is_string_value_writer_active {
            panic!("Incorrect writer usage: Cannot end object when string value writer is still active");
        }
        if !self.expects_member_name {
            panic!("Incorrect writer usage: Cannot end object when member value is expected");
        }
        self.on_container_end().await?;
        self.write_bytes(b"}").await
    }

    async fn begin_array(&mut self) -> Result<(), IoError> {
        self.before_value().await?;
        self.stack.push(StackValue::Array);
        self.is_empty = true;

        // Clear this because it is only relevant for objects; will be restored when entering parent object (if any) again
        self.expects_member_name = false;

        self.write_bytes(b"[").await
    }

    async fn end_array(&mut self) -> Result<(), IoError> {
        if !self.is_in_array() {
            panic!("Incorrect writer usage: Cannot end array when not inside array");
        }
        if self.is_string_value_writer_active {
            panic!(
                "Incorrect writer usage: Cannot end array when string value writer is still active"
            );
        }
        self.on_container_end().await?;
        self.write_bytes(b"]").await
    }

    async fn string_value(&mut self, value: &str) -> Result<(), IoError> {
        self.before_value().await?;
        self.write_string_value(value).await
    }

    async fn bool_value(&mut self, value: bool) -> Result<(), IoError> {
        self.before_value().await?;
        self.write_bytes(if value { b"true" } else { b"false" })
            .await
    }

    async fn null_value(&mut self) -> Result<(), IoError> {
        self.before_value().await?;
        self.write_bytes(b"null").await
    }

    async fn number_value<N: FiniteNumber + Display>(&mut self, value: N) -> Result<(), IoError> {
        let number_str = value.to_string();

        self.before_value().await?;
        self.write_bytes(number_str.as_bytes()).await
    }

    async fn fp_number_value<N: FloatingPointNumber + Display>(
        &mut self,
        value: N,
    ) -> Result<(), JsonNumberError> {
        value.validate()?;
        let number_str = value.to_string();
        self.before_value().await?;
        self.write_bytes(number_str.as_bytes()).await?;
        Ok(())
    }

    async fn number_value_from_string(&mut self, value: &str) -> Result<(), JsonNumberError> {
        if is_valid_json_number(value).await {
            self.before_value().await?;
            self.write_bytes(value.as_bytes()).await?;
            Ok(())
        } else {
            Err(JsonNumberError::InvalidNumber(format!(
                "invalid JSON number: {value}"
            )))
        }
    }

    #[cfg(feature = "serde")]
    async fn serialize_value<S: serde::ser::Serialize>(
        &mut self,
        value: &S,
    ) -> Result<(), crate::serde::SerializerError> {
        // TODO: Provide this as default implementation? Remove implementation in custom_json_writer test then;
        // does not seem to be possible though because Self would have to be guaranteed to be `Sized`?
        // not sure if that should be enforced for the JsonWriter trait

        let mut serializer = crate::serde::JsonWriterSerializer::new(self);
        value.serialize(&mut serializer).await
        // TODO: Verify that value was properly serialized (only single value; no incomplete array or object)
        // might not be necessary because Serde's Serialize API enforces this
    }

    async fn finish_document(mut self) -> Result<(), IoError> {
        if self.is_string_value_writer_active {
            panic!("Incorrect writer usage: Cannot finish document when string value writer is still active");
        }
        if self.expects_member_name {
            panic!("Incorrect writer usage: Cannot finish document when member name is expected");
        }
        if self.stack.is_empty() {
            if self.is_empty {
                panic!("Incorrect writer usage: Cannot finish document when no value has been written yet");
            }
        } else {
            panic!("Incorrect writer usage: Cannot finish document when top-level value is not finished");
        }
        self.flush().await
    }

    async fn string_value_writer(
        &mut self,
    ) -> Result<impl StringValueWriter + Unpin + '_, IoError> {
        self.before_value().await?;
        self.write_bytes(b"\"").await?;
        self.is_string_value_writer_active = true;
        Ok(StringValueWriterImpl {
            json_writer: self,
            utf8_buf: [0; utf8::MAX_BYTES_PER_CHAR],
            utf8_pos: 0,
            utf8_expected_len: 0,
            error: None,
        })
    }
}

// TODO: Is there a way to have `W` only optionally implement `Debug`?
impl<W: AsyncWrite + Debug> Debug for JsonStreamWriter<W> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut debug_struct = f.debug_struct("JsonStreamWriter");
        debug_struct
            .field("writer", &self.writer)
            .field("buf_count", &self.buf_write_pos);

        fn limit_str_middle(s: &str) -> String {
            let chars_count = s.chars().count();

            let prefix_len = 25;
            let suffix_len = prefix_len;

            let max_len = 55;
            // Assert that `max_len` is large enough for splitting to be possible and worth it
            assert!(max_len > prefix_len + suffix_len);

            if chars_count <= max_len {
                return s.to_owned();
            }

            let prefix_end = s.char_indices().nth(prefix_len).unwrap().0;
            let prefix = &s[..prefix_end];

            // `suffix_len - 1` because `nth_back(0)` already returns inclusive index of first char
            let suffix_start = s.char_indices().nth_back(suffix_len - 1).unwrap().0;
            let suffix = &s[suffix_start..];

            format!("{prefix} ... {suffix}")
        }

        match std::str::from_utf8(&self.buf[..self.buf_write_pos]) {
            Ok(buf_string) => {
                debug_struct.field("buf_str", &limit_str_middle(buf_string));
            }
            // In case of error buffer was likely already flushed before and split valid UTF-8;
            // loop until start of next valid substring is found
            Err(_) => {
                let mut substring_start = self.buf_write_pos;
                let mut buf_string_suffix = None;

                for i in 1..self.buf_write_pos {
                    if let Ok(suffix) = std::str::from_utf8(&self.buf[i..self.buf_write_pos]) {
                        buf_string_suffix = Some(format!("...{}", &limit_str_middle(suffix)));
                        substring_start = i;
                        break;
                    }
                }

                // Only include the bytes which could not be decoded to string
                debug_struct.field("buf...", &&self.buf[..substring_start]);

                // If no valid suffix could be decoded use "..."
                debug_struct.field(
                    "buf_str",
                    &buf_string_suffix.unwrap_or_else(|| "...".to_owned()),
                );
            }
        }

        debug_struct
            .field("is_empty", &self.is_empty)
            .field("expects_member_name", &self.expects_member_name)
            .field("stack", &self.stack)
            .field(
                "is_string_value_writer_active",
                &self.is_string_value_writer_active,
            )
            .field("indentation_level", &self.indentation_level)
            .field("writer_settings", &self.writer_settings)
            .finish()
    }
}

struct StringValueWriterImpl<'j, W: AsyncWrite> {
    json_writer: &'j mut JsonStreamWriter<W>,
    /// Buffer used to store incomplete data of a UTF-8 multi-byte character provided by
    /// a user of this writer
    ///
    /// Buffering it is necessary to make sure it is valid UTF-8 data before writing it to the
    /// underlying `Write`.
    utf8_buf: [u8; utf8::MAX_BYTES_PER_CHAR],
    /// Index (0-based) within [utf8_buf] where the next byte should be written, respectively
    /// number of already written bytes
    utf8_pos: usize,
    /// Expected number of total bytes for the character whose bytes are currently in [utf8_buf]
    utf8_expected_len: usize,
    /// The last error which occurred, and which should be returned for every subsequent `write` call
    // `io::Error` does not implement Clone, so this only contains some of its data
    error: Option<(ErrorKind, String)>,
}

fn map_utf8_error(e: Utf8Error) -> IoError {
    IoError::new(ErrorKind::InvalidData, e)
}

fn decode_utf8_char(bytes: &[u8]) -> Result<&str, IoError> {
    match std::str::from_utf8(bytes) {
        Err(e) => Err(map_utf8_error(e)),
        Ok(s) => {
            debug_assert!(s.chars().count() == 1);
            Ok(s)
        }
    }
}

impl<W: AsyncWrite + Unpin> StringValueWriterImpl<'_, W> {
    async fn write_impl(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }

        let mut start_pos = 0;
        if self.utf8_pos > 0 {
            let copy_count = (self.utf8_expected_len - self.utf8_pos).min(buf.len());
            self.utf8_buf[self.utf8_pos..(self.utf8_pos + copy_count)]
                .copy_from_slice(&buf[..copy_count]);
            self.utf8_pos += copy_count;

            if self.utf8_pos >= self.utf8_expected_len {
                self.utf8_pos = 0;
                let s = decode_utf8_char(&self.utf8_buf[..self.utf8_expected_len])?;
                self.json_writer.write_string_value_piece(s).await?;
            }
            start_pos += copy_count;
        }

        fn max_or_offset_negative(a: usize, b: usize, b_neg_off: usize) -> usize {
            debug_assert!(b >= a);
            // Avoids numeric underflow compared to normal `a.max(b - b_neg_off)`
            if b_neg_off > b {
                a
            } else {
                b - b_neg_off
            }
        }

        // Checks for incomplete UTF-8 data and converts the bytes with str::from_utf8
        let mut i = max_or_offset_negative(start_pos, buf.len(), utf8::MAX_BYTES_PER_CHAR);
        while i < buf.len() {
            let byte = buf[i];

            if !utf8::is_1byte(byte) {
                let expected_bytes_count;
                if utf8::is_2byte_start(byte) {
                    expected_bytes_count = 2;
                } else if utf8::is_3byte_start(byte) {
                    expected_bytes_count = 3;
                } else if utf8::is_4byte_start(byte) {
                    expected_bytes_count = 4;
                } else if utf8::is_continuation(byte) {
                    // Matched UTF-8 multi-byte continuation byte; continue to find start of next char
                    i += 1;
                    continue;
                } else {
                    return Err(IoError::new(ErrorKind::InvalidData, "invalid UTF-8 data"));
                }

                let remaining_count = buf.len() - i;
                if remaining_count < expected_bytes_count {
                    self.json_writer
                        .write_string_value_piece(
                            std::str::from_utf8(&buf[start_pos..i]).map_err(map_utf8_error)?,
                        )
                        .await?;

                    // Store the incomplete UTF-8 bytes in buffer
                    self.utf8_expected_len = expected_bytes_count;
                    self.utf8_pos = remaining_count;
                    self.utf8_buf[..remaining_count].copy_from_slice(&buf[i..]);
                    return Ok(buf.len());
                } else {
                    // Skip over the bytes; - 1 because loop iteration will perform + 1
                    i += expected_bytes_count - 1;
                }
            }
            // Check next byte (if any)
            i += 1;
        }

        self.json_writer
            .write_string_value_piece(
                std::str::from_utf8(&buf[start_pos..]).map_err(map_utf8_error)?,
            )
            .await?;
        Ok(buf.len())
    }

    fn check_previous_error(&self) -> std::io::Result<()> {
        match &self.error {
            None => Ok(()),
            // Report as `Other` kind (and with custom message) to avoid caller indefinitely retrying
            // because it considers the original error kind as safe to retry
            Some(e) => Err(IoError::other(format!(
                "previous error '{}': {}",
                e.0,
                e.1.clone()
            ))),
        }
    }
}
impl<W: AsyncWrite + Unpin> AsyncWrite for StringValueWriterImpl<'_, W> {
    fn poll_write(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        self.check_previous_error()?;

        let result = match std::pin::pin!(self.write_impl(buf)).poll(cx) {
            std::task::Poll::Ready(res) => res,
            p @ std::task::Poll::Pending => return p,
        };
        if let Err(e) = &result {
            self.error = Some((e.kind(), e.to_string()));
        }
        std::task::Poll::Ready(result)
    }

    fn poll_flush(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        self.check_previous_error()?;
        std::pin::pin!(self.json_writer.flush()).poll(cx)
    }

    fn poll_close(
        self: std::pin::Pin<&mut Self>,
        _cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::task::Poll::Ready(Ok(()))
    }
}

impl<W: AsyncWrite + Unpin> StringValueWriter for StringValueWriterImpl<'_, W> {
    // Provides more efficient implementation which benefits from avoided UTF-8 validation
    async fn write_str(&mut self, s: &str) -> Result<(), IoError> {
        self.check_previous_error()?;

        if self.utf8_pos > 0 {
            // If there is pending incomplete UTF-8 data, then this is an error because str contains
            // self-contained complete UTF-8 data, and therefore does not complete the incomplete data
            return Err(IoError::new(
                ErrorKind::InvalidData,
                "incomplete multi-byte UTF-8 data",
            ));
        }
        self.json_writer.write_string_value_piece(s).await?;
        Ok(())
    }

    async fn finish_value(self) -> Result<(), IoError> {
        self.check_previous_error()?;

        if self.utf8_pos > 0 {
            return Err(IoError::new(
                ErrorKind::InvalidData,
                "incomplete multi-byte UTF-8 data",
            ));
        }
        self.json_writer.write_bytes(b"\"").await?;
        self.json_writer.is_string_value_writer_active = false;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    type TestResult = Result<(), Box<dyn std::error::Error>>;

    #[futures_test::test]
    async fn numbers() -> TestResult {
        let mut writer = Vec::<u8>::new();
        let mut json_writer = JsonStreamWriter::new(&mut writer);

        json_writer.begin_array().await?;
        json_writer.number_value(8_u8).await?;
        json_writer.number_value(-8_i8).await?;
        json_writer.number_value(16_u16).await?;
        json_writer.number_value(-16_i16).await?;
        json_writer.number_value(32_u32).await?;
        json_writer.number_value(-32_i32).await?;
        json_writer.number_value(64_u64).await?;
        json_writer.number_value(-64_i64).await?;
        json_writer.number_value(128_u128).await?;
        json_writer.number_value(-128_i128).await?;

        json_writer.fp_number_value(1.5_f32).await?;
        json_writer.fp_number_value(-1.5_f32).await?;
        json_writer.fp_number_value(2.5_f64).await?;
        json_writer.fp_number_value(-2.5_f64).await?;

        json_writer.number_value_from_string("123.45e-12").await?;

        json_writer.end_array().await?;
        json_writer.finish_document().await?;

        assert_eq!(
            "[8,-8,16,-16,32,-32,64,-64,128,-128,1.5,-1.5,2.5,-2.5,123.45e-12]",
            String::from_utf8(writer)?
        );
        Ok(())
    }

    #[futures_test::test]
    async fn numbers_invalid() {
        fn assert_invalid_number(result: Result<(), JsonNumberError>, expected_message: &str) {
            match result {
                Ok(_) => panic!("Should have failed"),
                Err(e) => match e {
                    JsonNumberError::InvalidNumber(message) => {
                        assert_eq!(expected_message, message)
                    }
                    JsonNumberError::IoError(e) => panic!("Unexpected error: {e:?}"),
                },
            }
        }

        let mut writer = Vec::<u8>::new();
        let mut json_writer = JsonStreamWriter::new(&mut writer);

        assert_invalid_number(
            json_writer.fp_number_value(f32::NAN).await,
            &format!("non-finite number: {}", f32::NAN),
        );
        assert_invalid_number(
            json_writer.fp_number_value(f64::INFINITY).await,
            &format!("non-finite number: {}", f64::INFINITY),
        );
        assert_invalid_number(
            json_writer.number_value_from_string("NaN").await,
            "invalid JSON number: NaN",
        );
        assert_invalid_number(
            json_writer.number_value_from_string("+1").await,
            "invalid JSON number: +1",
        );
        assert_invalid_number(
            json_writer.number_value_from_string("00").await,
            "invalid JSON number: 00",
        );
        assert_invalid_number(
            json_writer.number_value_from_string("1e").await,
            "invalid JSON number: 1e",
        );
        assert_invalid_number(
            json_writer.number_value_from_string("12a").await,
            "invalid JSON number: 12a",
        );
    }

    #[futures_test::test]
    async fn literals() -> TestResult {
        let mut writer = Vec::<u8>::new();
        let mut json_writer = JsonStreamWriter::new(&mut writer);
        json_writer.begin_array().await?;

        json_writer.bool_value(true).await?;
        json_writer.bool_value(false).await?;
        json_writer.null_value().await?;

        json_writer.end_array().await?;
        json_writer.finish_document().await?;

        assert_eq!("[true,false,null]", String::from_utf8(writer)?);
        Ok(())
    }

    #[futures_test::test]
    async fn arrays() -> TestResult {
        let mut writer = Vec::<u8>::new();
        let mut json_writer = JsonStreamWriter::new(&mut writer);
        json_writer.begin_array().await?;

        json_writer.begin_array().await?;
        json_writer.number_value(1).await?;
        json_writer.end_array().await?;

        json_writer.begin_array().await?;
        json_writer.end_array().await?;

        json_writer.end_array().await?;
        json_writer.finish_document().await?;

        assert_eq!("[[1],[]]", String::from_utf8(writer)?);
        Ok(())
    }

    #[futures_test::test]
    #[should_panic(expected = "Incorrect writer usage: Cannot end array when not inside array")]
    async fn end_array_not_in_array() {
        let mut writer = Vec::<u8>::new();
        let mut json_writer = JsonStreamWriter::new(&mut writer);
        json_writer.begin_object().await.unwrap();

        json_writer.end_array().await.unwrap();
    }

    #[futures_test::test]
    async fn objects() -> TestResult {
        let mut writer = Vec::<u8>::new();
        let mut json_writer = JsonStreamWriter::new(&mut writer);
        json_writer.begin_object().await?;

        json_writer.name("a").await?;
        json_writer.number_value(1).await?;

        json_writer.name("").await?;
        json_writer.number_value(2).await?;

        json_writer.name("a").await?;

        json_writer.begin_object().await?;
        json_writer.name("c").await?;
        json_writer.begin_object().await?;
        json_writer.end_object().await?;
        json_writer.end_object().await?;

        json_writer.end_object().await?;
        json_writer.finish_document().await?;

        assert_eq!(r#"{"a":1,"":2,"a":{"c":{}}}"#, String::from_utf8(writer)?);
        Ok(())
    }

    #[futures_test::test]
    #[should_panic(expected = "Incorrect writer usage: Cannot end object when not inside object")]
    async fn end_object_not_in_object() {
        let mut writer = Vec::<u8>::new();
        let mut json_writer = JsonStreamWriter::new(&mut writer);
        json_writer.begin_array().await.unwrap();

        json_writer.end_object().await.unwrap();
    }

    #[futures_test::test]
    #[should_panic(
        expected = "Incorrect writer usage: Cannot end object when member value is expected"
    )]
    async fn end_object_expecting_value() {
        let mut writer = Vec::<u8>::new();
        let mut json_writer = JsonStreamWriter::new(&mut writer);
        json_writer.begin_object().await.unwrap();
        json_writer.name("a").await.unwrap();

        json_writer.end_object().await.unwrap();
    }

    #[futures_test::test]
    async fn arrays_objects_mixed() -> TestResult {
        let mut writer = Vec::<u8>::new();
        let mut json_writer = JsonStreamWriter::new(&mut writer);
        json_writer.begin_object().await?;

        json_writer.name("a").await?;

        json_writer.begin_object().await?;
        json_writer.name("b").await?;

        json_writer.begin_array().await?;

        json_writer.begin_object().await?;
        json_writer.end_object().await?;

        json_writer.begin_object().await?;
        json_writer.name("c").await?;
        json_writer.begin_array().await?;
        json_writer.begin_array().await?;
        json_writer.end_array().await?;
        json_writer.end_array().await?;
        json_writer.end_object().await?;

        json_writer.end_array().await?;
        json_writer.name("d").await?;
        json_writer.begin_array().await?;
        json_writer.end_array().await?;
        json_writer.end_object().await?;

        json_writer.end_object().await?;
        json_writer.finish_document().await?;

        assert_eq!(
            r#"{"a":{"b":[{},{"c":[[]]}],"d":[]}}"#,
            String::from_utf8(writer)?
        );
        Ok(())
    }

    #[futures_test::test]
    async fn strings() -> TestResult {
        let mut writer = Vec::<u8>::new();
        let mut json_writer = JsonStreamWriter::new(&mut writer);
        json_writer.begin_array().await?;

        json_writer.string_value("").await?;
        json_writer.string_value("ab").await?;
        json_writer.string_value("\u{0000}\u{001F}").await?;
        json_writer.string_value("a b").await?;
        json_writer
            .string_value("\"\\/\u{0008}\u{000C}\n\r\t")
            .await?;

        json_writer.string_value("\u{10FFFF}").await?;

        json_writer.end_array().await?;
        json_writer.finish_document().await?;

        assert_eq!(
            r#"["","ab","\u0000\u001F","a b","\"\\/\b\f\n\r\t","#.to_owned() + "\"\u{10FFFF}\"]",
            String::from_utf8(writer)?
        );
        Ok(())
    }

    #[futures_test::test]
    async fn string_writer() -> TestResult {
        let mut writer = Vec::<u8>::new();
        let mut json_writer = JsonStreamWriter::new(&mut writer);
        json_writer.begin_array().await?;

        let mut string_writer = json_writer.string_value_writer().await?;
        assert_eq!(0, string_writer.write(b"").await?);
        string_writer.write_all(b"a b").await?;
        string_writer.write_all(b"\x00\x1F").await?;
        string_writer.write_all(b"\"\\/\x08\x0C\n\r\t").await?;
        string_writer
            .write_all("\u{007F}\u{10FFFF}".as_bytes())
            .await?;

        // Split bytes of multi-byte UTF-8, writing each byte separately
        let bytes = "\u{007F}\u{0080}\u{07FF}\u{0800}\u{FFFF}\u{10000}\u{10FFFF}".as_bytes();
        for b in bytes {
            string_writer.write_all(&[*b]).await?;
        }
        string_writer.finish_value().await?;

        // Mix `write_all` and `write_str`
        let mut string_writer = json_writer.string_value_writer().await?;
        string_writer.write_all("\u{10FFFF}".as_bytes()).await?;
        string_writer.write_str("a \u{10FFFF}").await?;
        string_writer.write_all("b".as_bytes()).await?;
        string_writer.write_str("").await?; // empty string
        string_writer.write_all("c".as_bytes()).await?;
        string_writer.finish_value().await?;

        // Write an empty string
        let string_writer = json_writer.string_value_writer().await?;
        string_writer.finish_value().await?;

        json_writer.end_array().await?;
        json_writer.finish_document().await?;
        assert_eq!(
            r#"["a b\u0000\u001F\"\\/\b\f\n\r\t"#.to_owned()
                + "\u{007F}\u{10FFFF}\u{007F}\u{0080}\u{07FF}\u{0800}\u{FFFF}\u{10000}\u{10FFFF}\",\"\u{10FFFF}a \u{10FFFF}bc\",\"\"]",
            String::from_utf8(writer)?
        );
        Ok(())
    }

    #[futures_test::test]
    async fn string_writer_flush() -> TestResult {
        let mut writer = Vec::<u8>::new();
        let mut json_writer = JsonStreamWriter::new(&mut writer);

        let mut string_writer = json_writer.string_value_writer().await?;
        string_writer.write_all(b"abcd").await?;
        string_writer.flush().await?;
        string_writer.write_all(b"efgh").await?;
        string_writer.flush().await?;
        drop(string_writer);

        assert_eq!("\"abcdefgh", String::from_utf8(writer)?);
        Ok(())
    }

    #[futures_test::test]
    async fn string_writer_invalid() {
        // Uses macro instead of function with FnOnce(Box<...>) as parameter to avoid issues with
        // calling `StringValueWriter::finish_value` consuming `self`, see https://stackoverflow.com/q/46620790
        macro_rules! assert_invalid_utf8 {
            (
                |$string_value_writer:ident| $writing_expr:expr,
                $expected_custom_message:expr, // Option<&str>
            ) => {
                let mut writer = Vec::<u8>::new();
                let mut json_writer = JsonStreamWriter::new(&mut writer);
                let mut $string_value_writer = json_writer.string_value_writer().await.unwrap();

                // Use a closure here to allow `$writing_expr` to use the `?` operator for error handling
                #[allow(unused_mut)] // only for some callers the closure has to be mutable
                let mut writing_function = || {
                    $writing_expr
                };
                // Explicitly specify expression type to avoid callers having to specify it
                let expected_custom_message: Option<&str> = $expected_custom_message;

                let res: Result<(), IoError> = writing_function().await;
                match res {
                    Ok(_) => panic!("Should have failed"),
                    Err(e) => {
                        assert_eq!(ErrorKind::InvalidData, e.kind());

                        match expected_custom_message {
                            // None if error message should not be compared, e.g. because it is created by Rust and might change
                            None => assert!(
                                e.get_ref().unwrap().is::<Utf8Error>(),
                                "Inner error is not Utf8Error"
                            ),
                            Some(message) => {
                                assert_eq!(message, e.to_string(), "Custom message does not match")
                            }
                        }
                    }
                }
            }
        }

        assert_invalid_utf8!(
            |w| async move {
                // Invalid UTF-8 byte 1111_1000
                w.write_all(b"\xF8").await
            },
            Some("invalid UTF-8 data"),
        );

        assert_invalid_utf8!(
            |w| async move {
                // Malformed UTF-8; high surrogate U+D800 encoded in UTF-8 (= invalid)
                w.write_all(b"\xED\xA0\x80").await
            },
            None,
        );

        assert_invalid_utf8!(
            |w| async move {
                // Greater than max code point U+10FFFF; split across multiple bytes
                w.write_all(b"\xF4").await?;
                w.write_all(b"\x90").await?;
                w.write_all(b"\x80").await?;
                w.write_all(b"\x80").await
            },
            None,
        );

        assert_invalid_utf8!(
            |w| async move {
                // Overlong encoding for two bytes
                w.write_all(b"\xC1\xBF").await
            },
            None,
        );

        assert_invalid_utf8!(
            |w| async move {
                // Incomplete four bytes
                w.write_all(b"\xF0").await?;
                w.write_all(b"\x90").await?;
                w.write_all(b"\x80").await?;
                w.finish_value().await
            },
            Some("incomplete multi-byte UTF-8 data"),
        );

        assert_invalid_utf8!(
            |w| async move {
                // Leading continuation byte
                w.write_all(b"\x80").await?;
                w.finish_value().await
            },
            None,
        );

        assert_invalid_utf8!(
            |w| async move {
                // Too many continuation bytes
                w.write_all(b"\xC2").await?;
                w.write_all(b"\x80").await?;
                w.write_all(b"\x80").await?;
                w.finish_value().await
            },
            None,
        );

        assert_invalid_utf8!(
            |w| async move {
                // Incomplete multi-byte followed by `write_str`
                w.write_all(b"\xF0").await?;
                w.write_str("").await?; // even empty string should trigger this error
                w.finish_value().await
            },
            Some("incomplete multi-byte UTF-8 data"),
        );
    }

    #[futures_test::test]
    async fn string_writer_repeats_error() -> TestResult {
        let mut writer = Vec::<u8>::new();
        let mut json_writer = JsonStreamWriter::new(&mut writer);

        let mut string_writer = json_writer.string_value_writer().await?;
        // Invalid UTF-8 byte 1111_1000
        match string_writer.write_all(b"\xF8").await {
            Ok(_) => panic!("Should have failed"),
            Err(e) => {
                assert_eq!(ErrorKind::InvalidData, e.kind());
                // The wrapped error is actually the String message converted using `impl From<String> for Box<dyn Error>`
                let wrapped_error = e.get_ref().unwrap();
                assert_eq!("invalid UTF-8 data", wrapped_error.to_string());
            }
        }

        // Subsequent write attemps should fail with same error, but use custom message and kind `Other`
        fn assert_error(result: std::io::Result<()>) {
            match result {
                Ok(_) => panic!("Should have failed"),
                Err(e) => {
                    assert_eq!(ErrorKind::Other, e.kind());
                    // The wrapped error is actually the String message converted using `impl From<String> for Box<dyn Error>`
                    let wrapped_error = e.get_ref().unwrap();

                    let expected_original_kind = ErrorKind::InvalidData;
                    let expected_original_message = "invalid UTF-8 data";
                    assert_eq!(
                        format!(
                            "previous error '{}': {}",
                            expected_original_kind, expected_original_message
                        ),
                        wrapped_error.to_string()
                    );
                }
            }
        }

        assert_error(string_writer.write_all(b"test").await);
        assert_error(string_writer.write_str("test").await);
        assert_error(string_writer.flush().await);
        assert_error(string_writer.finish_value().await);

        // Should still consider string value writer as active because value was not
        // successfully finished
        assert!(json_writer.is_string_value_writer_active);

        Ok(())
    }

    #[futures_test::test]
    #[should_panic(
        expected = "Incorrect writer usage: Cannot end array when string value writer is still active"
    )]
    async fn string_writer_array_incomplete() {
        let mut writer = Vec::<u8>::new();
        let mut json_writer = JsonStreamWriter::new(&mut writer);
        json_writer.begin_array().await.unwrap();

        let string_writer = json_writer.string_value_writer().await.unwrap();
        drop(string_writer);

        json_writer.end_array().await.unwrap();
    }

    #[futures_test::test]
    #[should_panic(
        expected = "Incorrect writer usage: Cannot end object when string value writer is still active"
    )]
    async fn string_writer_object_incomplete() {
        let mut writer = Vec::<u8>::new();
        let mut json_writer = JsonStreamWriter::new(&mut writer);
        json_writer.begin_object().await.unwrap();
        json_writer.name("a").await.unwrap();

        let string_writer = json_writer.string_value_writer().await.unwrap();
        drop(string_writer);

        json_writer.end_object().await.unwrap();
    }

    #[futures_test::test]
    #[should_panic(
        expected = "Incorrect writer usage: Cannot finish document when string value writer is still active"
    )]
    async fn string_writer_incomplete_top_level() {
        let mut writer = Vec::<u8>::new();
        let mut json_writer = JsonStreamWriter::new(&mut writer);

        let string_writer = json_writer.string_value_writer().await.unwrap();
        drop(string_writer);

        json_writer.finish_document().await.unwrap();
    }

    #[futures_test::test]
    #[should_panic(expected = "Incorrect writer usage: Cannot write value when name is expected")]
    async fn string_writer_for_name() {
        let mut writer = Vec::<u8>::new();
        let mut json_writer = JsonStreamWriter::new(&mut writer);
        json_writer.begin_object().await.unwrap();

        json_writer.string_value_writer().await.unwrap();
    }

    #[futures_test::test]
    #[should_panic(
        expected = "Incorrect writer usage: Cannot write name when name is not expected"
    )]
    async fn name_as_value() {
        let mut writer = Vec::<u8>::new();
        let mut json_writer = JsonStreamWriter::new(&mut writer);

        json_writer.name("test").await.unwrap();
    }

    #[futures_test::test]
    #[should_panic(
        expected = "Incorrect writer usage: Cannot write name when name is not expected"
    )]
    async fn name_as_member_value() {
        let mut writer = Vec::<u8>::new();
        let mut json_writer = JsonStreamWriter::new(&mut writer);
        json_writer.begin_object().await.unwrap();
        json_writer.name("a").await.unwrap();

        json_writer.name("test").await.unwrap();
    }

    #[futures_test::test]
    async fn automatic_buffer_flush() -> TestResult {
        let mut writer = Vec::<u8>::new();
        let mut json_writer = JsonStreamWriter::new(&mut writer);
        json_writer
            .string_value("abc\"def".repeat(WRITER_BUF_SIZE).as_str())
            .await?;
        json_writer.finish_document().await?;

        assert_eq!(
            format!("\"{}\"", "abc\\\"def".repeat(WRITER_BUF_SIZE)),
            String::from_utf8(writer)?
        );
        Ok(())
    }

    #[futures_test::test]
    async fn multiple_top_level() -> TestResult {
        fn create_writer<W: AsyncWrite>(
            writer: W,
            top_level_separator: &str,
        ) -> JsonStreamWriter<W> {
            JsonStreamWriter::new_custom(
                writer,
                WriterSettings {
                    multi_top_level_value_separator: Some(top_level_separator.to_owned()),
                    ..Default::default()
                },
            )
        }

        let mut writer = Vec::<u8>::new();
        let mut json_writer = create_writer(&mut writer, "");
        json_writer.begin_array().await?;
        json_writer.end_array().await?;
        json_writer.finish_document().await?;
        assert_eq!("[]", String::from_utf8(writer)?);

        let mut writer = Vec::<u8>::new();
        let mut json_writer = create_writer(&mut writer, "");
        json_writer.begin_array().await?;
        json_writer.end_array().await?;
        json_writer.begin_array().await?;
        json_writer.end_array().await?;
        json_writer.finish_document().await?;
        assert_eq!("[][]", String::from_utf8(writer)?);

        let mut writer = Vec::<u8>::new();
        let mut json_writer = create_writer(&mut writer, "#\n#");
        json_writer.begin_array().await?;
        json_writer.end_array().await?;
        json_writer.begin_array().await?;
        json_writer.end_array().await?;
        json_writer.finish_document().await?;
        assert_eq!("[]#\n#[]", String::from_utf8(writer)?);

        Ok(())
    }

    #[futures_test::test]
    #[should_panic(
        expected = "Incorrect writer usage: Cannot write multiple top-level values when not enabled in writer settings"
    )]
    async fn multiple_top_level_disallowed() {
        let mut writer = Vec::<u8>::new();
        let mut json_writer = JsonStreamWriter::new(&mut writer);
        json_writer.bool_value(true).await.unwrap();

        json_writer.bool_value(false).await.unwrap();
    }

    #[futures_test::test]
    async fn pretty_print() -> TestResult {
        let mut writer = Vec::<u8>::new();
        let mut json_writer = JsonStreamWriter::new_custom(
            &mut writer,
            WriterSettings {
                pretty_print: true,
                multi_top_level_value_separator: Some("#".to_owned()),
                ..Default::default()
            },
        );

        json_writer.begin_array().await?;

        json_writer.begin_array().await?;
        json_writer.end_array().await?;

        json_writer.begin_array().await?;
        json_writer.number_value(1).await?;
        json_writer.end_array().await?;

        json_writer.begin_object().await?;

        json_writer.name("a").await?;
        json_writer.begin_array().await?;
        json_writer.begin_object().await?;
        json_writer.name("b").await?;
        json_writer.number_value(2).await?;
        json_writer.end_object().await?;
        json_writer.begin_object().await?;
        json_writer.end_object().await?;
        json_writer.end_array().await?;

        json_writer.name("c").await?;
        json_writer.number_value(3).await?;

        json_writer.end_object().await?;

        json_writer.end_array().await?;

        json_writer.begin_array().await?;
        json_writer.number_value(4).await?;
        json_writer.end_array().await?;

        json_writer.finish_document().await?;

        assert_eq!(
            "[\n  [],\n  [\n    1\n  ],\n  {\n    \"a\": [\n      {\n        \"b\": 2\n      },\n      {}\n    ],\n    \"c\": 3\n  }\n]#[\n  4\n]",
            String::from_utf8(writer)?
        );
        Ok(())
    }

    #[futures_test::test]
    async fn escape_all_control_chars() -> TestResult {
        let mut writer = Vec::<u8>::new();
        let mut json_writer = JsonStreamWriter::new_custom(
            &mut writer,
            WriterSettings {
                escape_all_control_chars: true,
                ..Default::default()
            },
        );

        json_writer
            .string_value("\u{0000}\u{001F} test \" \u{007E}\u{007F}\u{0080}\u{009F}\u{00A0}")
            .await?;

        json_writer.finish_document().await?;
        assert_eq!(
            "\"\\u0000\\u001F test \\\" \u{007E}\\u007F\\u0080\\u009F\u{00A0}\"",
            String::from_utf8(writer)?
        );
        Ok(())
    }

    #[futures_test::test]
    async fn escape_all_non_ascii() -> TestResult {
        let mut writer = Vec::<u8>::new();
        let mut json_writer = JsonStreamWriter::new_custom(
            &mut writer,
            WriterSettings {
                escape_all_non_ascii: true,
                ..Default::default()
            },
        );
        json_writer
            .string_value("\u{0000}\u{001F} test \" \u{007F}\u{0080}\u{10000}\u{10FFFF}")
            .await?;
        json_writer.finish_document().await?;
        assert_eq!(
            "\"\\u0000\\u001F test \\\" \u{007F}\\u0080\\uD800\\uDC00\\uDBFF\\uDFFF\"",
            String::from_utf8(writer)?
        );

        let mut writer = Vec::<u8>::new();
        let mut json_writer = JsonStreamWriter::new_custom(
            &mut writer,
            WriterSettings {
                escape_all_control_chars: true,
                escape_all_non_ascii: true,
                ..Default::default()
            },
        );
        json_writer
            .string_value("\u{0000} test \" \u{007F}\u{0080}\u{10FFFF}")
            .await?;
        json_writer.finish_document().await?;
        assert_eq!(
            "\"\\u0000 test \\\" \\u007F\\u0080\\uDBFF\\uDFFF\"",
            String::from_utf8(writer)?
        );

        Ok(())
    }

    #[futures_test::test]
    #[should_panic(
        expected = "Incorrect writer usage: Cannot finish document when no value has been written yet"
    )]
    async fn finish_empty_document() {
        let mut writer = Vec::<u8>::new();
        let json_writer = JsonStreamWriter::new(&mut writer);

        json_writer.finish_document().await.unwrap();
    }

    #[futures_test::test]
    #[should_panic(
        expected = "Incorrect writer usage: Cannot finish document when top-level value is not finished"
    )]
    async fn finish_incomplete_document() {
        let mut writer = Vec::<u8>::new();
        let mut json_writer = JsonStreamWriter::new(&mut writer);
        json_writer.begin_array().await.unwrap();

        json_writer.finish_document().await.unwrap();
    }

    /* // TODO
    /// Writer which returns `ErrorKind::Interrupted` most of the time
    struct InterruptedWriter {
        buf: Vec<u8>,
        // For every write attempt return `ErrorKind::Interrupted` a few times before performing write
        interrupted_count: u32,
    }
    impl InterruptedWriter {
        pub fn new() -> Self {
            InterruptedWriter {
                buf: Vec::new(),
                interrupted_count: 0,
            }
        }

        pub fn get_written_string(self) -> String {
            String::from_utf8(self.buf).unwrap()
        }
    }
    impl AsyncWrite for InterruptedWriter {
        fn poll_write(
            mut self: std::pin::Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
            buf: &[u8],
        ) -> std::task::Poll<std::io::Result<usize>> {
            if buf.is_empty() {
                return std::task::Poll::Ready(Ok(0));
            }

            if self.interrupted_count >= 3 {
                self.interrupted_count = 0;
                // Only write a single byte
                self.buf.push(buf[0]);
                std::task::Poll::Ready(Ok(1))
            } else {
                self.interrupted_count += 1;
                std::task::Poll::Ready(Err(IoError::from(ErrorKind::Interrupted)))
            }
        }

        fn poll_flush(
            self: std::pin::Pin<&mut Self>,
            cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<std::io::Result<()>> {
            // Do nothing
            std::task::Poll::Ready(Ok(()))
        }

        fn poll_close(
            self: std::pin::Pin<&mut Self>,
            cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<std::io::Result<()>> {
            // Do nothing
            std::task::Poll::Ready(Ok(()))
        }
    }

    /// String value writer must not return (or rather propagate) `ErrorKind::Interrupted`;
    /// otherwise most `Write` methods will re-attempt the write even though the underlying
    /// JSON stream writer is in an inconsistent state (e.g. incomplete escape sequence
    /// having been written).
    #[futures_test::test]
    async fn string_writer_interrupted() -> TestResult {
        let mut writer = InterruptedWriter::new();
        let mut json_writer = JsonStreamWriter::new(&mut writer);

        let mut string_writer = json_writer.string_value_writer().await?;
        let string_bytes = "test \" \u{10FFFF}".as_bytes();
        match string_writer.write(string_bytes).await {
            // Current implementation should have written complete buf content (this is not a requirement of `Write::write` though)
            Ok(n) => assert_eq!(string_bytes.len(), n),
            // For current implementation no error should have occurred
            // Especially regardless of implemention, `ErrorKind::Interrupted` must not have been returned
            r => panic!("Unexpected result: {r:?}"),
        }

        string_writer.finish_value().await?;
        json_writer.finish_document().await?;
        assert_eq!("\"test \\\" \u{10FFFF}\"", writer.get_written_string());

        Ok(())
    }

    /// JSON stream writer should continuously retry writing in case underlying `Write`
    /// returns `ErrorKind::Interrupted`.
    #[futures_test::test]
    async fn writer_interrupted() -> TestResult {
        let mut writer = InterruptedWriter::new();
        let mut json_writer = JsonStreamWriter::new(&mut writer);

        json_writer.begin_array().await?;
        json_writer.bool_value(true).await?;
        json_writer.number_value_from_string("123.4e5").await?;
        json_writer.string_value("test \" 1 \u{10FFFF}").await?;

        let mut string_writer = json_writer.string_value_writer().await?;
        string_writer
            .write_all("test \" 2 \u{10FFFF}, ".as_bytes())
            .await?;
        string_writer.write_str("test \" 3 \u{10FFFF}").await?;
        string_writer.finish_value().await?;

        json_writer.end_array().await?;
        json_writer.finish_document().await?;

        assert_eq!(
            "[true,123.4e5,\"test \\\" 1 \u{10FFFF}\",\"test \\\" 2 \u{10FFFF}, test \\\" 3 \u{10FFFF}\"]",
            writer.get_written_string()
        );
        Ok(())
    }
    */

    struct DebuggableWriter;

    impl AsyncWrite for DebuggableWriter {
        fn poll_write(
            self: std::pin::Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
            buf: &[u8],
        ) -> std::task::Poll<std::io::Result<usize>> {
            // Pretend complete buffer content was written
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

    impl Debug for DebuggableWriter {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(f, "debuggable-writer")
        }
    }

    fn new_with_debuggable_writer() -> JsonStreamWriter<DebuggableWriter> {
        JsonStreamWriter::new(DebuggableWriter)
    }

    // The following Debug output tests mainly exist to make sure the buffer content is properly displayed
    // Besides that they heavily rely on implementation details

    #[futures_test::test]
    async fn debug_writer() -> TestResult {
        let mut json_writer = new_with_debuggable_writer();
        assert_eq!(
            "JsonStreamWriter { writer: debuggable-writer, buf_count: 0, buf_str: \"\", is_empty: true, expects_member_name: false, stack: [], is_string_value_writer_active: false, indentation_level: 0, writer_settings: WriterSettings { pretty_print: false, escape_all_control_chars: false, escape_all_non_ascii: false, multi_top_level_value_separator: None } }",
            format!("{json_writer:?}")
        );

        json_writer.string_value("test").await?;
        assert_eq!(
            "JsonStreamWriter { writer: debuggable-writer, buf_count: 6, buf_str: \"\\\"test\\\"\", is_empty: false, expects_member_name: false, stack: [], is_string_value_writer_active: false, indentation_level: 0, writer_settings: WriterSettings { pretty_print: false, escape_all_control_chars: false, escape_all_non_ascii: false, multi_top_level_value_separator: None } }",
            format!("{json_writer:?}")
        );
        Ok(())
    }

    #[futures_test::test]
    async fn debug_writer_long() -> TestResult {
        let mut json_writer = new_with_debuggable_writer();
        json_writer
            .string_value("test".repeat(100).as_str())
            .await?;
        assert_eq!(
            "JsonStreamWriter { writer: debuggable-writer, buf_count: 402, buf_str: \"\\\"testtesttesttesttesttest ... testtesttesttesttesttest\\\"\", is_empty: false, expects_member_name: false, stack: [], is_string_value_writer_active: false, indentation_level: 0, writer_settings: WriterSettings { pretty_print: false, escape_all_control_chars: false, escape_all_non_ascii: false, multi_top_level_value_separator: None } }",
            format!("{json_writer:?}"
        ));
        Ok(())
    }

    #[futures_test::test]
    async fn debug_writer_incomplete_with_suffix() -> TestResult {
        let mut json_writer = new_with_debuggable_writer();
        // Write a string value which splits a multi-byte UTF-8 char
        // `WRITER_BUF_SIZE - 2` due to leading '"' of string, and to leave one byte space for
        // first byte of multi-byte UTF-8
        let string_value = format!("{}\u{10FFFF}test", "a".repeat(WRITER_BUF_SIZE - 2));
        json_writer.string_value(&string_value).await?;
        assert_eq!(
            "JsonStreamWriter { writer: debuggable-writer, buf_count: 8, buf...: [143, 191, 191], buf_str: \"...test\\\"\", is_empty: false, expects_member_name: false, stack: [], is_string_value_writer_active: false, indentation_level: 0, writer_settings: WriterSettings { pretty_print: false, escape_all_control_chars: false, escape_all_non_ascii: false, multi_top_level_value_separator: None } }",
            format!("{json_writer:?}")
        );
        Ok(())
    }

    #[futures_test::test]
    async fn debug_writer_incomplete_with_long_suffix() -> TestResult {
        let mut json_writer = new_with_debuggable_writer();
        // Write a string value which splits a multi-byte UTF-8 char
        // `WRITER_BUF_SIZE - 2` due to leading '"' of string, and to leave one byte space for
        // first byte of multi-byte UTF-8
        let string_value = format!(
            "{}\u{10FFFF}{}",
            "a".repeat(WRITER_BUF_SIZE - 2),
            "test".repeat(100)
        );
        json_writer.string_value(&string_value).await?;
        assert_eq!(
            "JsonStreamWriter { writer: debuggable-writer, buf_count: 404, buf...: [143, 191, 191], buf_str: \"...testtesttesttesttesttestt ... testtesttesttesttesttest\\\"\", is_empty: false, expects_member_name: false, stack: [], is_string_value_writer_active: false, indentation_level: 0, writer_settings: WriterSettings { pretty_print: false, escape_all_control_chars: false, escape_all_non_ascii: false, multi_top_level_value_separator: None } }",
            format!("{json_writer:?}")
        );
        Ok(())
    }

    #[cfg(feature = "serde")]
    mod serde {
        use super::*;
        use crate::serde::SerializerError;
        use ::serde::{ser::SerializeStruct, Serialize, Serializer};
        use std::collections::HashMap;

        #[futures_test::test]
        async fn serialize_value() -> TestResult {
            let mut writer = Vec::<u8>::new();
            let mut json_writer = JsonStreamWriter::new(&mut writer);
            json_writer.begin_object().await?;
            json_writer.name("outer").await?;

            struct CustomStruct;
            impl Serialize for CustomStruct {
                fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
                    let mut struc = serializer.serialize_struct("name", 3)?;
                    struc.serialize_field("a", &1)?;
                    struc.serialize_field("b", &HashMap::from([("key", "value")]))?;
                    struc.serialize_field("c", &[1, 2])?;
                    struc.end()
                }
            }
            json_writer.serialize_value(&CustomStruct).await?;

            json_writer.end_object().await?;

            json_writer.finish_document().await?;
            assert_eq!(
                r#"{"outer":{"a":1,"b":{"key":"value"},"c":[1,2]}}"#,
                String::from_utf8(writer)?
            );

            Ok(())
        }

        #[futures_test::test]
        async fn serialize_value_invalid() {
            let mut json_writer = JsonStreamWriter::new(futures::io::sink());
            let number = f32::NAN;
            match json_writer.serialize_value(&number) {
                Err(SerializerError::InvalidNumber(message)) => {
                    assert_eq!(format!("non-finite number: {number}"), message);
                }
                r => panic!("Unexpected result: {r:?}"),
            }
        }

        #[futures_test::test]
        #[should_panic(
            expected = "Incorrect writer usage: Cannot write value when name is expected"
        )]
        async fn serialize_value_no_value_expected() {
            let mut json_writer = JsonStreamWriter::new(futures::io::sink());
            json_writer.begin_object().await.unwrap();

            json_writer.serialize_value(&"test").unwrap();
        }
    }
}
