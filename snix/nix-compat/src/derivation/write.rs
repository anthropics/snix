//! This module implements the serialisation of derivations into the
//! [ATerm][] format used by C++ Nix.
//!
//! [ATerm]: http://program-transformation.org/Tools/ATermFormat.html

use crate::aterm::write_escaped;
use crate::derivation::{DerivationBuilder, OutputName, Outputs, OutputsBuilder};
use crate::nixhash::Sha256;
use crate::store_path::{StorePath, StorePathRef};
use data_encoding::HEXLOWER;
use smol_str::format_smolstr;

use std::{collections::BTreeSet, io, io::Error, io::Write};

pub const DERIVATION_PREFIX: &str = "Derive";
pub const PAREN_OPEN: char = '(';
pub const PAREN_CLOSE: char = ')';
pub const BRACKET_OPEN: char = '[';
pub const BRACKET_CLOSE: char = ']';
pub const COMMA: char = ',';
pub const QUOTE: char = '"';

/// Something that can be written as ATerm.
///
/// Note that we mostly use explicit `write_*` calls
/// instead since the serialization of the items depends on
/// the context a lot.
pub(super) trait AtermWriteable {
    fn aterm_write(&self, writer: &mut impl io::Write) -> std::io::Result<()>;
}
impl<F: AtermWriteable> AtermWriteable for &F {
    fn aterm_write(&self, writer: &mut impl io::Write) -> std::io::Result<()> {
        (**self).aterm_write(writer)
    }
}

impl<'a> AtermWriteable for StorePathRef<'a> {
    fn aterm_write(&self, writer: &mut impl Write) -> std::io::Result<()> {
        write_char(writer, QUOTE)?;
        write!(writer, "{}", self.as_absolute_path_fmt())?;
        write_char(writer, QUOTE)?;
        Ok(())
    }
}

impl AtermWriteable for StorePath {
    fn aterm_write(&self, writer: &mut impl Write) -> std::io::Result<()> {
        self.as_ref().aterm_write(writer)
    }
}

impl AtermWriteable for String {
    fn aterm_write(&self, writer: &mut impl Write) -> std::io::Result<()> {
        write_field(writer, self, true)
    }
}

impl AtermWriteable for &str {
    fn aterm_write(&self, writer: &mut impl Write) -> std::io::Result<()> {
        write_field(writer, self, true)
    }
}

impl AtermWriteable for [u8] {
    fn aterm_write(&self, writer: &mut impl Write) -> std::io::Result<()> {
        write_field(writer, HEXLOWER.encode(self), false)
    }
}

impl AtermWriteable for Sha256 {
    fn aterm_write(&self, writer: &mut impl Write) -> std::io::Result<()> {
        write_field(writer, format_smolstr!("{self:x}").as_bytes(), false)
    }
}

impl AtermWriteable for OutputName {
    fn aterm_write(&self, writer: &mut impl io::Write) -> std::io::Result<()> {
        write_field(writer, self.as_ref(), false)
    }
}

impl AtermWriteable for OutputsBuilder {
    fn aterm_write(&self, writer: &mut impl io::Write) -> std::io::Result<()> {
        write_char(writer, BRACKET_OPEN)?;
        match self {
            OutputsBuilder::Fixed(output_hash) => {
                write_char(writer, PAREN_OPEN)?;

                write_array_elements(
                    writer,
                    [
                        OutputName::out().as_str(),
                        "",
                        output_hash.as_mode_and_algo_str(),
                        &data_encoding::HEXLOWER.encode(output_hash.hash.digest_as_bytes()),
                    ],
                )?;

                write_char(writer, PAREN_CLOSE)?;
            }
            OutputsBuilder::InputAddressed(output_names) => {
                for (ii, output_name) in output_names.iter().enumerate() {
                    if ii > 0 {
                        write_char(writer, COMMA)?;
                    }

                    write_char(writer, PAREN_OPEN)?;
                    write_array_elements(writer, [output_name.as_str(), "", "", ""])?;
                    write_char(writer, PAREN_CLOSE)?;
                }
            }
        }
        write_char(writer, BRACKET_CLOSE)?;

        Ok(())
    }
}

impl AtermWriteable for Outputs {
    fn aterm_write(&self, writer: &mut impl io::Write) -> std::io::Result<()> {
        write_char(writer, BRACKET_OPEN)?;
        if let Some((output_hash, store_path)) = self.as_fixed_output() {
            let path_str = store_path.to_absolute_path();

            write_char(writer, PAREN_OPEN)?;
            write_array_elements(
                writer,
                [
                    OutputName::out().as_str(),
                    &path_str,
                    output_hash.as_mode_and_algo_str(),
                    &data_encoding::HEXLOWER.encode(output_hash.hash.digest_as_bytes()),
                ],
            )?;
            write_char(writer, PAREN_CLOSE)?;
        } else {
            for (ii, (output_name, store_path)) in self.iter().enumerate() {
                let path_str = store_path.to_absolute_path();
                if ii > 0 {
                    write_char(writer, COMMA)?;
                }

                write_char(writer, PAREN_OPEN)?;
                write_array_elements(writer, [output_name.as_str(), &path_str, "", ""])?;
                write_char(writer, PAREN_CLOSE)?;
            }
        }
        write_char(writer, BRACKET_CLOSE)?;

        Ok(())
    }
}

impl DerivationBuilder {
    /// Like `serialize`, but allows replacing the environment, outputs and input_derivations for hash calculations.
    ///
    /// This is used to render the ATerm representation of a Derivation "modulo
    /// fixed-output derivations".
    ///
    /// The passed environment, outputs and input_derivations MUST be sorted.
    pub(super) fn serialize_with_replacements<'a, E, EK, EV, O, I, ID>(
        &self,
        writer: &mut impl std::io::Write,
        environment: E,
        outputs: O,
        input_derivations_sorted: I,
    ) -> Result<(), io::Error>
    where
        E: IntoIterator<Item = (EK, EV)>,
        EK: AsRef<[u8]>,
        EV: AsRef<[u8]>,
        O: AtermWriteable,
        I: Iterator<Item = (ID, &'a BTreeSet<OutputName>)>,
        ID: AtermWriteable,
    {
        writer.write_all(DERIVATION_PREFIX.as_bytes())?;
        write_char(writer, PAREN_OPEN)?;

        outputs.aterm_write(writer)?;
        write_char(writer, COMMA)?;

        write_input_derivations(writer, input_derivations_sorted)?;
        write_char(writer, COMMA)?;

        write_input_sources(writer, &self.input_sources)?;
        write_char(writer, COMMA)?;

        write_system(writer, &self.system)?;
        write_char(writer, COMMA)?;

        write_builder(writer, &self.command)?;
        write_char(writer, COMMA)?;

        write_arguments(writer, &self.arguments)?;
        write_char(writer, COMMA)?;

        write_environment(writer, environment)?;

        write_char(writer, PAREN_CLOSE)?;

        Ok(())
    }
}

/// Writes a character to the writer.
pub(crate) fn write_char(writer: &mut impl Write, c: char) -> io::Result<()> {
    let mut buf = [0; 4];
    let b = c.encode_utf8(&mut buf).as_bytes();
    writer.write_all(b)
}

/// Write a string `s` as a quoted field to the writer.
/// The `escape` argument controls whether escaping will be skipped.
/// This is the case if `s` is known to only contain characters that need no
/// escaping.
pub(crate) fn write_field<S: AsRef<[u8]>>(
    writer: &mut impl Write,
    s: S,
    escape: bool,
) -> io::Result<()> {
    write_char(writer, QUOTE)?;

    if !escape {
        writer.write_all(s.as_ref())?;
    } else {
        write_escaped(s, writer)?;
    }

    write_char(writer, QUOTE)?;

    Ok(())
}

fn write_array_elements<S>(
    writer: &mut impl Write,
    elements: impl IntoIterator<Item = S>,
) -> Result<(), io::Error>
where
    S: AtermWriteable,
{
    for (index, element) in elements.into_iter().enumerate() {
        if index > 0 {
            write_char(writer, COMMA)?;
        }

        element.aterm_write(writer)?;
    }

    Ok(())
}

fn write_input_derivations<'a, I, K>(
    writer: &mut impl Write,
    input_derivations_sorted: I,
) -> Result<(), io::Error>
where
    I: Iterator<Item = (K, &'a BTreeSet<OutputName>)>,
    K: AtermWriteable,
{
    write_char(writer, BRACKET_OPEN)?;

    for (ii, (k, output_names)) in input_derivations_sorted.enumerate() {
        if ii > 0 {
            write_char(writer, COMMA)?;
        }

        write_char(writer, PAREN_OPEN)?;
        k.aterm_write(writer)?;
        write_char(writer, COMMA)?;

        write_char(writer, BRACKET_OPEN)?;
        write_array_elements(writer, output_names)?;
        write_char(writer, BRACKET_CLOSE)?;

        write_char(writer, PAREN_CLOSE)?;
    }

    write_char(writer, BRACKET_CLOSE)?;

    Ok(())
}

fn write_input_sources(
    writer: &mut impl Write,
    input_sources: &BTreeSet<StorePath>,
) -> Result<(), io::Error> {
    write_char(writer, BRACKET_OPEN)?;
    write_array_elements(writer, input_sources)?;
    write_char(writer, BRACKET_CLOSE)?;

    Ok(())
}

fn write_system(writer: &mut impl Write, platform: &str) -> Result<(), Error> {
    write_field(writer, platform, true)?;
    Ok(())
}

fn write_builder(writer: &mut impl Write, builder: &str) -> Result<(), Error> {
    write_field(writer, builder, true)?;
    Ok(())
}

fn write_arguments(writer: &mut impl Write, arguments: &[String]) -> Result<(), io::Error> {
    write_char(writer, BRACKET_OPEN)?;
    write_array_elements(writer, arguments)?;
    write_char(writer, BRACKET_CLOSE)?;

    Ok(())
}

fn write_environment<E, K, V>(writer: &mut impl Write, environment: E) -> Result<(), io::Error>
where
    E: IntoIterator<Item = (K, V)>,
    K: AsRef<[u8]>,
    V: AsRef<[u8]>,
{
    write_char(writer, BRACKET_OPEN)?;

    for (i, (k, v)) in environment.into_iter().enumerate() {
        if i > 0 {
            write_char(writer, COMMA)?;
        }

        write_char(writer, PAREN_OPEN)?;
        write_field(writer, k, false)?;
        write_char(writer, COMMA)?;
        write_field(writer, v, true)?;
        write_char(writer, PAREN_CLOSE)?;
    }

    write_char(writer, BRACKET_CLOSE)?;

    Ok(())
}

/// Returns an iterator combining two other iterators (sorted by K).
///
/// If K is found in the shadow iterator (B), the element will be yielded from B,
/// else from A.
///
/// This allows computing environment variables without having to clone and
/// mutate the entire environment variables struct (see [DerivationBuilder::hash_derivation_modulo]).
pub(super) fn shadow<A, B, K, V>(main: A, shadow: B) -> ShadowIter<A::IntoIter, B::IntoIter>
where
    K: Ord,
    A: IntoIterator<Item = (K, V)>,
    B: IntoIterator<Item = (K, V)>,
{
    ShadowIter {
        main: main.into_iter().peekable(),
        shadow: shadow.into_iter().peekable(),
    }
}

/// Iterator for the [shadow] function.
pub(super) struct ShadowIter<A: Iterator, B: Iterator> {
    main: std::iter::Peekable<A>,
    shadow: std::iter::Peekable<B>,
}

impl<A, B, K, V> Iterator for ShadowIter<A, B>
where
    K: Ord,
    A: Iterator<Item = (K, V)>,
    B: Iterator<Item = (K, V)>,
{
    type Item = (K, V);

    fn next(&mut self) -> Option<Self::Item> {
        match (self.main.peek(), self.shadow.peek()) {
            (Some((main_k, _)), Some((shadow_k, _))) if main_k < shadow_k => self.main.next(),
            (Some((main_k, _)), Some((shadow_k, _))) if main_k == shadow_k => {
                self.main.next();
                self.shadow.next()
            }
            (_, Some(_)) => self.shadow.next(),
            (Some(_), None) => self.main.next(),
            (None, None) => None,
        }
    }
}
