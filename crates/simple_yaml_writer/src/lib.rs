//! A library for writing YAML documents with proper formatting, indentation, and quoting.
//!
//! This library provides a structured way to create valid YAML output without
//! having to worry about proper indentation, quoting, or formatting. It offers
//! a builder-style API for constructing YAML documents programmatically with full control
//! over how structures are formatted.
//!
//! # Example
//!
//! ```
//! use simple_yaml_writer::YamlWriter;
//!
//! # fn main() -> std::io::Result<()> {
//! // Create a writer with any type that implements Write
//! let mut writer = YamlWriter::new(Vec::new());
//!
//! // Get the root table to start writing YAML content
//! let mut root = writer.root();
//!
//! // Write a comment at the top of the yaml
//! root.comment("This is a YAML document")?;
//!
//! // Add key-value pairs to the root
//! root.string("name", "example-project")?;
//! root.string("version", "1.0.0")?;
//! root.boolean("bool", true)?;
//! root.number("number", 3.0)?;
//! root.null("nil")?;
//!
//! // Add a nested table
//! root.table("dependencies", |deps| {
//!     deps.string("serde", "1.0.136")?;
//!     deps.string("rand", "0.8.5")?;
//!     Ok(())
//! })?;
//!
//! // Add a sequence (array)
//! root.sequence("authors", |seq| {
//!     seq.string("Jane Doe")?;
//!     seq.string("John Smith")?;
//!     Ok(())
//! })?;
//!
//! // Add an inline table for compact representation
//! root.inline_table("metadata", |meta| {
//!     meta.string("type", "library")?;
//!     meta.boolean("public", true)?;
//!     Ok(())
//! })?;
//!
//! // Finish writing and extract the result
//! let buffer = writer.finish();
//! let yaml_output = String::from_utf8(buffer).expect("Must be valid UTF-8");
//!
//! // Check the complete output matches the expected YAML
//! let expected_output = r#"# This is a YAML document
//! name: example-project
//! version: 1.0.0
//! bool: true
//! number: 3
//! nil: null
//! dependencies:
//!   serde: 1.0.136
//!   rand: 0.8.5
//! authors:
//!   - Jane Doe
//!   - John Smith
//! metadata: { type: library, public: true }
//! "#;
//! assert_eq!(yaml_output, expected_output);
//! # Ok(())
//! # }
//! ```
//!
//! The crate provides several writer types for different YAML structures:
//! - `YamlWriter`: The main entry point for creating YAML documents
//! - `YamlTable`: For writing key-value mappings with proper indentation
//! - `YamlSequence`: For writing sequences (arrays) with items on separate lines
//! - `YamlInlineTable`: For writing compact tables on a single line
//! - `YamlInlineSequence`: For writing compact sequences on a single line

use std::io::Write;

#[derive(Copy, Clone, Eq, PartialEq)]
enum FirstKeyState {
    /// The entry is the first entry but the indentation is already applied by
    /// the parent element.
    Inline,

    /// This is the first entry in the table, normal indentation should be applied.
    First,

    /// This is not the first entry in the table, indentation should be applied.
    NotFirst,
}

/// A writer for creating valid YAML documents.
///
/// The `YamlWriter` provides a structured way to create YAML output without
/// having to worry about proper indentation, quoting, or formatting.
pub struct YamlWriter<W: Write> {
    writer: W,
}

impl<W: Write> YamlWriter<W> {
    /// Creates a new YAML writer that writes to the given destination.
    ///
    /// # Arguments
    ///
    /// * `writer` - The destination to write the YAML content to.
    pub fn new(writer: W) -> Self {
        YamlWriter { writer }
    }

    /// Creates a root table for the YAML document.
    ///
    /// This is the starting point for creating a YAML document. All content
    /// must be added through this root table.
    ///
    /// # Returns
    ///
    /// A table writer for the root level of the YAML document.
    pub fn root(&mut self) -> YamlTable<'_, W> {
        YamlTable {
            writer: &mut self.writer,
            indent: "".to_string(),
            first_key: FirstKeyState::First,
        }
    }

    /// Finishes writing and returns the underlying writer.
    ///
    /// Call this method when you're done writing the YAML document to get
    /// back the original writer.
    pub fn finish(self) -> W {
        self.writer
    }
}

fn needs_quotes(s: &str) -> bool {
    if s.is_empty() {
        return true;
    }
    let lower = s.to_lowercase();
    if ["true", "false", "yes", "no", "on", "off", "~"].contains(&lower.as_str()) {
        return true;
    }
    if s.parse::<f64>().is_ok() {
        return true;
    }
    if s.contains(": ") {
        return true;
    }
    false
}

fn write_quoted<W: Write>(writer: &mut W, s: &str) -> std::io::Result<()> {
    if needs_quotes(s) {
        write!(writer, "\"{}\"", s)
    } else {
        write!(writer, "{}", s)
    }
}

/// A YAML table (mapping) writer.
///
/// This struct allows writing key-value pairs to a YAML mapping with proper
/// indentation and formatting. Tables can contain string values, numbers,
/// booleans, nested tables, sequences, and inline variants of these.
pub struct YamlTable<'a, W: Write> {
    writer: &'a mut W,
    indent: String,
    first_key: FirstKeyState,
}

impl<'a, W: Write> YamlTable<'a, W> {
    fn indent(&mut self) -> std::io::Result<()> {
        if self.first_key != FirstKeyState::Inline {
            write!(self.writer, "{}", self.indent)?;
        }
        self.first_key = FirstKeyState::NotFirst;
        Ok(())
    }

    /// Adds a string key-value pair to the table.
    ///
    /// # Arguments
    ///
    /// * `key` - The key for the string value
    /// * `value` - The string value to add
    ///
    /// # Returns
    ///
    /// A result indicating success or an I/O error
    pub fn string(&mut self, key: &str, value: &str) -> std::io::Result<()> {
        self.indent()?;
        write_quoted(&mut self.writer, key)?;
        write!(self.writer, ": ")?;
        write_quoted(&mut self.writer, value)?;
        writeln!(self.writer)?;
        Ok(())
    }

    /// Adds a number key-value pair to the table.
    ///
    /// # Arguments
    ///
    /// * `key` - The key for the numeric value
    /// * `value` - The numeric value to add
    ///
    /// # Returns
    ///
    /// A result indicating success or an I/O error
    pub fn number(&mut self, key: &str, value: f64) -> std::io::Result<()> {
        self.indent()?;
        write_quoted(&mut self.writer, key)?;
        write!(self.writer, ": ")?;
        write!(self.writer, "{}", value)?;
        writeln!(self.writer)?;
        Ok(())
    }

    /// Adds a boolean key-value pair to the table.
    ///
    /// # Arguments
    ///
    /// * `key` - The key for the boolean value
    /// * `value` - The boolean value to add
    ///
    /// # Returns
    ///
    /// A result indicating success or an I/O error
    pub fn boolean(&mut self, key: &str, value: bool) -> std::io::Result<()> {
        self.indent()?;
        write_quoted(&mut self.writer, key)?;
        write!(self.writer, ": {}", value)?;
        writeln!(self.writer)?;
        Ok(())
    }

    /// Adds a null value with the specified key to the table.
    ///
    /// # Arguments
    ///
    /// * `key` - The key for the null value
    ///
    /// # Returns
    ///
    /// A result indicating success or an I/O error
    pub fn null(&mut self, key: &str) -> std::io::Result<()> {
        self.indent()?;
        write_quoted(&mut self.writer, key)?;
        write!(self.writer, ": null")?;
        writeln!(self.writer)?;
        Ok(())
    }

    /// Adds a comment line to the table.
    ///
    /// # Arguments
    ///
    /// * `comment` - The comment text (without the leading '#')
    ///
    /// # Returns
    ///
    /// A result indicating success or an I/O error
    pub fn comment(&mut self, comment: &str) -> std::io::Result<()> {
        self.indent()?;
        write!(self.writer, "# {}", comment)?;
        writeln!(self.writer)?;
        Ok(())
    }

    /// Adds an inline table as a value for the given key.
    ///
    /// An inline table is written on a single line with curly braces: `{ key1: value1, key2: value2 }`.
    ///
    /// # Arguments
    ///
    /// * `key` - The key for the inline table
    /// * `f` - A function that will be called with a `YamlInlineTable` to populate the table
    ///
    /// # Returns
    ///
    /// A result indicating success or an I/O error
    pub fn inline_table<F>(&mut self, key: &str, f: F) -> std::io::Result<()>
    where
        F: FnOnce(&mut YamlInlineTable<'_, W>) -> std::io::Result<()>,
    {
        self.indent()?;
        write_quoted(&mut self.writer, key)?;
        write!(self.writer, ": {{")?;
        let mut inline_table = YamlInlineTable {
            writer: self.writer,
            first_pair: true,
        };
        f(&mut inline_table)?;
        writeln!(self.writer, " }}")?;
        Ok(())
    }

    /// Adds a nested table as a value for the given key.
    ///
    /// The nested table will be properly indented and can contain any valid YAML content.
    ///
    /// # Arguments
    ///
    /// * `key` - The key for the nested table
    /// * `f` - A function that will be called with a `YamlTable` to populate the table
    ///
    /// # Returns
    ///
    /// A result indicating success or an I/O error
    pub fn table<F>(&mut self, key: &str, f: F) -> std::io::Result<()>
    where
        F: FnOnce(&mut YamlTable<'_, W>) -> std::io::Result<()>,
    {
        self.indent()?;
        write_quoted(&mut self.writer, key)?;
        writeln!(self.writer, ":")?;
        let new_indent = format!("{}  ", self.indent);
        let mut obj = YamlTable {
            writer: self.writer,
            indent: new_indent,
            first_key: FirstKeyState::First,
        };
        f(&mut obj)?;
        Ok(())
    }

    /// Adds a sequence (array) as a value for the given key.
    ///
    /// The sequence is written with each item on a new line, preceded by a dash.
    ///
    /// # Arguments
    ///
    /// * `key` - The key for the sequence
    /// * `f` - A function that will be called with a `YamlSequence` to populate the sequence
    ///
    /// # Returns
    ///
    /// A result indicating success or an I/O error
    pub fn sequence<F>(&mut self, key: &str, f: F) -> std::io::Result<()>
    where
        F: FnOnce(&mut YamlSequence<'_, W>) -> std::io::Result<()>,
    {
        self.indent()?;
        write_quoted(&mut self.writer, key)?;
        writeln!(self.writer, ":")?;
        let mut seq = YamlSequence {
            writer: self.writer,
        };
        f(&mut seq)?;
        Ok(())
    }

    /// Adds an inline sequence (array) as a value for the given key.
    ///
    /// An inline sequence is written on a single line with square brackets: `[ item1, item2 ]`.
    ///
    /// # Arguments
    ///
    /// * `key` - The key for the inline sequence
    /// * `f` - A function that will be called with a `YamlInlineSequence` to populate the sequence
    ///
    /// # Returns
    ///
    /// A result indicating success or an I/O error
    pub fn inline_sequence<F>(&mut self, key: &str, f: F) -> std::io::Result<()>
    where
        F: FnOnce(&mut YamlInlineSequence<'_, W>) -> std::io::Result<()>,
    {
        self.indent()?;
        write_quoted(&mut self.writer, key)?;
        write!(self.writer, ": [")?;
        let mut inline_seq = YamlInlineSequence {
            writer: self.writer,
            first_item: true,
        };
        f(&mut inline_seq)?;
        writeln!(self.writer, " ]")?;
        Ok(())
    }
}

/// A writer for inline YAML tables.
///
/// Inline tables are written on a single line with curly braces: `{ key1: value1, key2: value2 }`.
/// This struct provides methods to add various types of values to an inline table.
pub struct YamlInlineTable<'a, W: Write> {
    writer: &'a mut W,
    first_pair: bool,
}

impl<'a, W: Write> YamlInlineTable<'a, W> {
    fn seperator(&mut self) -> std::io::Result<()> {
        if !self.first_pair {
            write!(self.writer, ", ")?;
        } else {
            write!(self.writer, " ")?;
            self.first_pair = false;
        }
        Ok(())
    }

    /// Adds a string key-value pair to the inline table.
    ///
    /// # Arguments
    ///
    /// * `key` - The key for the string value
    /// * `value` - The string value to add
    ///
    /// # Returns
    ///
    /// A result indicating success or an I/O error
    pub fn string(&mut self, key: &str, value: &str) -> std::io::Result<()> {
        self.seperator()?;
        write_quoted(&mut self.writer, key)?;
        write!(self.writer, ": ")?;
        write_quoted(&mut self.writer, value)?;
        Ok(())
    }

    /// Adds a number key-value pair to the inline table.
    ///
    /// # Arguments
    ///
    /// * `key` - The key for the numeric value
    /// * `value` - The numeric value to add
    ///
    /// # Returns
    ///
    /// A result indicating success or an I/O error
    pub fn number(&mut self, key: &str, value: f64) -> std::io::Result<()> {
        self.seperator()?;
        write_quoted(&mut self.writer, key)?;
        write!(self.writer, ": ")?;
        write!(self.writer, "{}", value)?;
        Ok(())
    }

    /// Adds a boolean key-value pair to the inline table.
    ///
    /// # Arguments
    ///
    /// * `key` - The key for the boolean value
    /// * `value` - The boolean value to add
    ///
    /// # Returns
    ///
    /// A result indicating success or an I/O error
    pub fn boolean(&mut self, key: &str, value: bool) -> std::io::Result<()> {
        self.seperator()?;
        write_quoted(&mut self.writer, key)?;
        write!(self.writer, ": {}", value)?;
        Ok(())
    }

    /// Adds a null value with the specified key to the inline table.
    ///
    /// # Arguments
    ///
    /// * `key` - The key for the null value
    ///
    /// # Returns
    ///
    /// A result indicating success or an I/O error
    pub fn null(&mut self, key: &str) -> std::io::Result<()> {
        self.seperator()?;
        write_quoted(&mut self.writer, key)?;
        write!(self.writer, ": null")?;
        Ok(())
    }

    /// Adds an inline sequence as a value for the given key in the inline table.
    ///
    /// # Arguments
    ///
    /// * `key` - The key for the inline sequence
    /// * `f` - A function that will be called with a `YamlInlineSequence` to populate the sequence
    ///
    /// # Returns
    ///
    /// A result indicating success or an I/O error
    pub fn inline_sequence<F>(&mut self, key: &str, f: F) -> std::io::Result<()>
    where
        F: FnOnce(&mut YamlInlineSequence<'_, W>) -> std::io::Result<()>,
    {
        self.seperator()?;
        write_quoted(&mut self.writer, key)?;
        write!(self.writer, ": [")?;
        let mut inline_seq = YamlInlineSequence {
            writer: self.writer,
            first_item: true,
        };
        f(&mut inline_seq)?;
        write!(self.writer, " ]")?;
        Ok(())
    }
}

/// A writer for YAML sequences (arrays).
///
/// This struct provides methods to add various types of values to a YAML sequence,
/// where each item is on a new line and preceded by a dash.
pub struct YamlSequence<'a, W: Write> {
    writer: &'a mut W,
}

impl<'a, W: Write> YamlSequence<'a, W> {
    /// Adds a string item to the sequence.
    ///
    /// # Arguments
    ///
    /// * `item` - The string value to add to the sequence
    ///
    /// # Returns
    ///
    /// A result indicating success or an I/O error
    pub fn string(&mut self, item: &str) -> std::io::Result<()> {
        write!(self.writer, "  - ")?;
        write_quoted(&mut self.writer, item)?;
        writeln!(self.writer)?;
        Ok(())
    }

    /// Adds a number item to the sequence.
    ///
    /// # Arguments
    ///
    /// * `item` - The numeric value to add to the sequence
    ///
    /// # Returns
    ///
    /// A result indicating success or an I/O error
    pub fn number(&mut self, item: f64) -> std::io::Result<()> {
        writeln!(self.writer, "  - {}", item)?;
        Ok(())
    }

    /// Adds a boolean item to the sequence.
    ///
    /// # Arguments
    ///
    /// * `value` - The boolean value to add to the sequence
    ///
    /// # Returns
    ///
    /// A result indicating success or an I/O error
    pub fn boolean(&mut self, value: bool) -> std::io::Result<()> {
        writeln!(self.writer, "  - {}", value)?;
        Ok(())
    }

    /// Adds a null item to the sequence.
    ///
    /// # Returns
    ///
    /// A result indicating success or an I/O error
    pub fn null(&mut self) -> std::io::Result<()> {
        writeln!(self.writer, "  - null")?;
        Ok(())
    }

    /// Adds a comment line to the sequence.
    ///
    /// # Arguments
    ///
    /// * `comment` - The comment text (without the leading '#')
    ///
    /// # Returns
    ///
    /// A result indicating success or an I/O error
    pub fn comment(&mut self, comment: &str) -> std::io::Result<()> {
        writeln!(self.writer, "  # {}", comment)?;
        Ok(())
    }

    /// Adds an inline table item to the sequence.
    ///
    /// # Arguments
    ///
    /// * `f` - A function that will be called with a `YamlInlineTable` to populate the table
    ///
    /// # Returns
    ///
    /// A result indicating success or an I/O error
    pub fn inline_table<F>(&mut self, f: F) -> std::io::Result<()>
    where
        F: FnOnce(&mut YamlInlineTable<'_, W>) -> std::io::Result<()>,
    {
        write!(self.writer, "  - {{")?;
        let mut table = YamlInlineTable {
            writer: self.writer,
            first_pair: true,
        };
        f(&mut table)?;
        writeln!(self.writer, " }}")?;
        Ok(())
    }

    /// Adds a table item to the sequence.
    ///
    /// # Arguments
    ///
    /// * `f` - A function that will be called with a `YamlTable` to populate the table
    ///
    /// # Returns
    ///
    /// A result indicating success or an I/O error
    pub fn table<F>(&mut self, f: F) -> std::io::Result<()>
    where
        F: FnOnce(&mut YamlTable<'_, W>) -> std::io::Result<()>,
    {
        write!(self.writer, "  - ")?;
        let mut obj = YamlTable {
            writer: self.writer,
            indent: "    ".to_string(),
            first_key: FirstKeyState::Inline,
        };
        f(&mut obj)?;
        Ok(())
    }

    /// Adds an inline sequence item to the sequence.
    ///
    /// # Arguments
    ///
    /// * `f` - A function that will be called with a `YamlInlineSequence` to populate the nested sequence
    ///
    /// # Returns
    ///
    /// A result indicating success or an I/O error
    pub fn inline_sequence<F>(&mut self, f: F) -> std::io::Result<()>
    where
        F: FnOnce(&mut YamlInlineSequence<'_, W>) -> std::io::Result<()>,
    {
        write!(self.writer, "  - [")?;
        let mut inline_seq = YamlInlineSequence {
            writer: self.writer,
            first_item: true,
        };
        f(&mut inline_seq)?;
        writeln!(self.writer, " ]")?;
        Ok(())
    }
}

/// A writer for inline YAML sequences (arrays).
///
/// Inline sequences are written on a single line with square brackets: `[ item1, item2 ]`.
/// This struct provides methods to add various types of values to an inline sequence.
pub struct YamlInlineSequence<'a, W: Write> {
    writer: &'a mut W,
    first_item: bool,
}

impl<'a, W: Write> YamlInlineSequence<'a, W> {
    fn seperator(&mut self) -> std::io::Result<()> {
        if !self.first_item {
            write!(self.writer, ", ")?;
        } else {
            write!(self.writer, " ")?;
            self.first_item = false;
        }
        Ok(())
    }

    /// Adds a string item to the inline sequence.
    ///
    /// # Arguments
    ///
    /// * `value` - The string value to add to the sequence
    ///
    /// # Returns
    ///
    /// A result indicating success or an I/O error
    pub fn string(&mut self, value: &str) -> std::io::Result<()> {
        self.seperator()?;
        write_quoted(&mut self.writer, value)?;
        Ok(())
    }

    /// Adds a number item to the inline sequence.
    ///
    /// # Arguments
    ///
    /// * `value` - The numeric value to add to the sequence
    ///
    /// # Returns
    ///
    /// A result indicating success or an I/O error
    pub fn number(&mut self, value: f64) -> std::io::Result<()> {
        self.seperator()?;
        write!(self.writer, "{}", value)?;
        Ok(())
    }

    /// Adds a boolean item to the inline sequence.
    ///
    /// # Arguments
    ///
    /// * `value` - The boolean value to add to the sequence
    ///
    /// # Returns
    ///
    /// A result indicating success or an I/O error
    pub fn boolean(&mut self, value: bool) -> std::io::Result<()> {
        self.seperator()?;
        write!(self.writer, "{}", value)?;
        Ok(())
    }

    /// Adds a null item to the inline sequence.
    ///
    /// # Returns
    ///
    /// A result indicating success or an I/O error
    pub fn null(&mut self) -> std::io::Result<()> {
        self.seperator()?;
        write!(self.writer, "null")?;
        Ok(())
    }

    /// Adds an inline table item to the inline sequence.
    ///
    /// # Arguments
    ///
    /// * `f` - A function that will be called with a `YamlInlineTable` to populate the table
    ///
    /// # Returns
    ///
    /// A result indicating success or an I/O error
    pub fn inline_table<F>(&mut self, f: F) -> std::io::Result<()>
    where
        F: FnOnce(&mut YamlInlineTable<'_, W>) -> std::io::Result<()>,
    {
        self.seperator()?;
        write!(self.writer, "{{")?;
        let mut inline_table = YamlInlineTable {
            writer: self.writer,
            first_pair: true,
        };
        f(&mut inline_table)?;
        write!(self.writer, " }}")?;
        Ok(())
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use insta::assert_snapshot;

    #[test]
    fn test_table_writer() -> std::io::Result<()> {
        let mut yaml_writer = YamlWriter::new(Vec::new());
        let mut root = yaml_writer.root();
        root.string("key1", "value1")?;
        root.inline_table("key2", |table| {
            table.string("foo", "value2")?;
            table.string("bar", "value3")?;
            Ok(())
        })?;
        root.table("key6", |table| {
            table.string("foo", "value4")?;
            table.string("bar", "value5")?;
            Ok(())
        })?;
        root.sequence("key3", |seq| {
            seq.string("item1")?;
            seq.string("item2")?;
            seq.inline_table(|table| {
                table.string("foo", "value4")?;
                table.string("bar", "value5")?;
                Ok(())
            })?;
            seq.table(|table| {
                table.string("foo", "value4")?;
                table.string("bar", "value5")?;
                Ok(())
            })?;
            Ok(())
        })?;
        root.inline_sequence("key4", |seq| {
            seq.string("val")?;
            Ok(())
        })?;
        let result_buf = yaml_writer.finish();
        let yaml_str = String::from_utf8(result_buf).unwrap();
        assert_snapshot!(yaml_str, @r###"
key1: value1
key2: { foo: value2, bar: value3 }
key6:
  foo: value4
  bar: value5
key3:
  - item1
  - item2
  - { foo: value4, bar: value5 }
  - foo: value4
    bar: value5
key4: [ val ]
"###);
        Ok(())
    }

    #[test]
    fn test_root_components() -> std::io::Result<()> {
        let mut writer = YamlWriter::new(Vec::new());
        let mut root = writer.root();
        root.string("greeting", "hello world")?;
        root.inline_table("info", |table| {
            table.string("foo", "bar")?;
            table.string("baz", "qux")?;
            Ok(())
        })?;
        root.table("config", |table| {
            table.string("opt1", "true")?;
            table.string("opt2", "false")?;
            Ok(())
        })?;
        let result = String::from_utf8(writer.finish()).unwrap();
        assert_snapshot!(result, @r###"
greeting: hello world
info: { foo: bar, baz: qux }
config:
  opt1: "true"
  opt2: "false"
"###);
        Ok(())
    }

    #[test]
    fn test_sequence_components() -> std::io::Result<()> {
        let mut yaml_writer = YamlWriter::new(Vec::new());
        let mut root = yaml_writer.root();
        root.sequence("items", |seq| {
            seq.inline_table(|table| {
                table.string("key", "value")?;
                Ok(())
            })?;
            seq.table(|table| {
                table.string("name", "example")?;
                table.string("desc", "a test")?;
                Ok(())
            })?;
            Ok(())
        })?;
        let result = String::from_utf8(yaml_writer.finish()).unwrap();
        assert_snapshot!(result, @r###"
items:
  - { key: value }
  - name: example
    desc: a test
"###);
        Ok(())
    }

    #[test]
    fn test_empty_yaml() -> std::io::Result<()> {
        let yaml_writer = YamlWriter::new(Vec::new());
        let result = String::from_utf8(yaml_writer.finish()).unwrap();
        assert_snapshot!(result, @r###"
"###);
        Ok(())
    }

    #[test]
    fn test_deep_nesting() -> std::io::Result<()> {
        let mut writer = YamlWriter::new(Vec::new());
        let mut root = writer.root();
        root.table("level1", |lvl1| {
            lvl1.inline_table("level2_inline", |lvl2| {
                lvl2.string("keyA", "valA")?;
                lvl2.string("keyB", "valB")?;
                Ok(())
            })?;
            lvl1.sequence("level2_seq", |seq| {
                seq.string("item1")?;
                seq.table(|table| {
                    table.string("nestedKey", "nestedValue")?;
                    table.table("nestedTable", |nested| {
                        nested.string("deeper", "yes")?;
                        Ok(())
                    })?;
                    Ok(())
                })?;
                Ok(())
            })?;
            Ok(())
        })?;
        let result = String::from_utf8(writer.finish()).unwrap();
        assert_snapshot!(result, @r###"
level1:
  level2_inline: { keyA: valA, keyB: valB }
  level2_seq:
  - item1
  - nestedKey: nestedValue
    nestedTable:
      deeper: "yes"
"###);
        Ok(())
    }

    #[test]
    fn test_mixed_seq_tables() -> std::io::Result<()> {
        let mut writer = YamlWriter::new(Vec::new());
        let mut root = writer.root();
        root.sequence("mix", |seq| {
            seq.table(|table| {
                table.inline_table("nested", |table| {
                    table.string("a", "1")?;
                    table.string("b", "2")?;
                    Ok(())
                })?;
                Ok(())
            })?;
            Ok(())
        })?;
        let result = String::from_utf8(writer.finish()).unwrap();
        assert_snapshot!(result, @r###"
mix:
  - nested: { a: "1", b: "2" }
"###);
        Ok(())
    }

    #[test]
    fn test_quoting() -> std::io::Result<()> {
        let mut writer = YamlWriter::new(Vec::new());
        let mut root = writer.root();
        root.string("unquoted", "normal")?;
        root.string("bool", "true")?;
        root.string("number", "123.45")?;
        root.string("colon", "value: with colon")?;
        root.sequence("list", |seq| {
            seq.string("false")?;
            seq.string("456")?;
            seq.string("no colon")?;
            Ok(())
        })?;
        let yaml = String::from_utf8(writer.finish()).unwrap();
        assert_snapshot!(yaml, @r###"
unquoted: normal
bool: "true"
number: "123.45"
colon: "value: with colon"
list:
  - "false"
  - "456"
  - no colon
"###);
        Ok(())
    }

    #[test]
    fn test_inline_sequence_in_inline_table() -> std::io::Result<()> {
        let mut writer = YamlWriter::new(Vec::new());
        let mut root = writer.root();
        root.inline_table("is", |it| {
            it.inline_sequence("seq", |seq| {
                seq.string("item1")?;
                seq.inline_table(|it| {
                    it.string("inner", "value")?;
                    Ok(())
                })?;
                Ok(())
            })?;
            Ok(())
        })?;
        let result = String::from_utf8(writer.finish()).unwrap();
        assert_snapshot!(result, @r###"
is: { seq: [ item1, { inner: value } ] }
"###);
        Ok(())
    }

    #[test]
    fn test_number_functions() -> std::io::Result<()> {
        let mut writer = YamlWriter::new(Vec::new());
        let mut root = writer.root();
        root.number("num", 123.45)?;
        root.inline_table("inline_num", |table| {
            table.number("num", 67.89)?;
            Ok(())
        })?;
        root.sequence("seq_num", |seq| {
            seq.number(10.1)?;
            Ok(())
        })?;
        root.inline_sequence("inline_seq", |seq| {
            seq.number(2022.0)?;
            Ok(())
        })?;
        let result = String::from_utf8(writer.finish()).unwrap();
        assert_snapshot!(result, @r###"
num: 123.45
inline_num: { num: 67.89 }
seq_num:
  - 10.1
inline_seq: [ 2022 ]
"###);
        Ok(())
    }

    #[test]
    fn test_boolean_and_null_values() -> std::io::Result<()> {
        let mut writer = YamlWriter::new(Vec::new());
        let mut root = writer.root();

        // Test boolean values
        root.boolean("bool_true", true)?;
        root.boolean("bool_false", false)?;

        // Test null values
        root.null("null_value")?;

        // Test in table
        root.table("nested", |table| {
            table.boolean("inner_bool", true)?;
            table.null("inner_null")?;
            Ok(())
        })?;

        // Test in inline table
        root.inline_table("inline", |table| {
            table.boolean("bool", false)?;
            table.null("nil")?;
            Ok(())
        })?;

        // Test in sequence
        root.sequence("seq", |seq| {
            seq.boolean(true)?;
            seq.null()?;
            Ok(())
        })?;

        // Test in inline sequence
        root.inline_sequence("inline_seq", |seq| {
            seq.boolean(true)?;
            seq.null()?;
            seq.string("text")?;
            Ok(())
        })?;

        let result = String::from_utf8(writer.finish()).unwrap();
        assert_snapshot!(result, @r###"
bool_true: true
bool_false: false
null_value: null
nested:
  inner_bool: true
  inner_null: null
inline: { bool: false, nil: null }
seq:
  - true
  - null
inline_seq: [ true, null, text ]
"###);
        Ok(())
    }

    #[test]
    fn test_comments() -> std::io::Result<()> {
        let mut writer = YamlWriter::new(Vec::new());
        let mut root = writer.root();

        root.comment("This is a comment at the root level")?;
        root.string("key", "value")?;
        root.comment("Comment before a section")?;

        root.table("section", |table| {
            table.comment("Comment inside section")?;
            table.string("inner", "value")?;
            Ok(())
        })?;

        root.sequence("items", |seq| {
            seq.comment("Comment inside sequence")?;
            seq.string("item1")?;
            seq.string("item2")?;
            Ok(())
        })?;

        let result = String::from_utf8(writer.finish()).unwrap();
        assert_snapshot!(result, @r###"
# This is a comment at the root level
key: value
# Comment before a section
section:
  # Comment inside section
  inner: value
items:
  # Comment inside sequence
  - item1
  - item2
"###);
        Ok(())
    }
}
