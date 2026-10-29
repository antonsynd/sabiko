using System;
using System.IO;
using System.Globalization;
using System.Text;

namespace Sabiko.Runtime;

/// <summary>
/// Output primitives for Sabiko's <c>Print</c>/<c>PrintLine</c>. Formatting follows
/// Rust's <c>Display</c> exactly (decimal integers, lowercase <c>true</c>/<c>false</c>,
/// shortest round-trip floats without exponents), so a Sabiko program and the same
/// program compiled by rustc print identical bytes. Output goes through
/// <see cref="System.Console.Out"/> as UTF-8; <c>PrintLine</c> always ends the line
/// with <c>\n</c>, never <see cref="Environment.NewLine"/>.
/// </summary>
public static class Console
{
    private static readonly TextWriter Out = OpenOut();

    [ThreadStatic]
    private static char[]? t_buffer;

    private static TextWriter OpenOut()
    {
        // Console.Out already encodes as UTF-8 on Unix; on Windows it follows the console
        // code page unless told otherwise. 65001 is the UTF-8 code page.
        if (System.Console.OutputEncoding.CodePage != 65001)
        {
            System.Console.OutputEncoding = new UTF8Encoding(encoderShouldEmitUTF8Identifier: false);
        }
        return System.Console.Out;
    }

    private static char[] Buffer => t_buffer ??= new char[256];

    // ---- integers -----------------------------------------------------------------

    /// <summary>Writes an <c>i8</c>.</summary>
    public static void Print(sbyte value) => Write(value, newline: false);
    /// <summary>Writes an <c>i8</c> and a newline.</summary>
    public static void PrintLine(sbyte value) => Write(value, newline: true);

    /// <summary>Writes an <c>i16</c>.</summary>
    public static void Print(short value) => Write(value, newline: false);
    /// <summary>Writes an <c>i16</c> and a newline.</summary>
    public static void PrintLine(short value) => Write(value, newline: true);

    /// <summary>Writes an <c>i32</c>.</summary>
    public static void Print(int value) => Write(value, newline: false);
    /// <summary>Writes an <c>i32</c> and a newline.</summary>
    public static void PrintLine(int value) => Write(value, newline: true);

    /// <summary>Writes an <c>i64</c>.</summary>
    public static void Print(long value) => Write(value, newline: false);
    /// <summary>Writes an <c>i64</c> and a newline.</summary>
    public static void PrintLine(long value) => Write(value, newline: true);

    /// <summary>Writes an <c>isize</c>.</summary>
    public static void Print(nint value) => Write(value, newline: false);
    /// <summary>Writes an <c>isize</c> and a newline.</summary>
    public static void PrintLine(nint value) => Write(value, newline: true);

    /// <summary>Writes a <c>u8</c>.</summary>
    public static void Print(byte value) => Write(value, newline: false);
    /// <summary>Writes a <c>u8</c> and a newline.</summary>
    public static void PrintLine(byte value) => Write(value, newline: true);

    /// <summary>Writes a <c>u16</c>.</summary>
    public static void Print(ushort value) => Write(value, newline: false);
    /// <summary>Writes a <c>u16</c> and a newline.</summary>
    public static void PrintLine(ushort value) => Write(value, newline: true);

    /// <summary>Writes a <c>u32</c>.</summary>
    public static void Print(uint value) => Write(value, newline: false);
    /// <summary>Writes a <c>u32</c> and a newline.</summary>
    public static void PrintLine(uint value) => Write(value, newline: true);

    /// <summary>Writes a <c>u64</c>.</summary>
    public static void Print(ulong value) => Write(value, newline: false);
    /// <summary>Writes a <c>u64</c> and a newline.</summary>
    public static void PrintLine(ulong value) => Write(value, newline: true);

    /// <summary>Writes a <c>usize</c>.</summary>
    public static void Print(nuint value) => Write(value, newline: false);
    /// <summary>Writes a <c>usize</c> and a newline.</summary>
    public static void PrintLine(nuint value) => Write(value, newline: true);

    /// <summary>Writes an <c>i128</c>.</summary>
    public static void Print(Int128 value) => Write(value, newline: false);
    /// <summary>Writes an <c>i128</c> and a newline.</summary>
    public static void PrintLine(Int128 value) => Write(value, newline: true);

    /// <summary>Writes a <c>u128</c>.</summary>
    public static void Print(UInt128 value) => Write(value, newline: false);
    /// <summary>Writes a <c>u128</c> and a newline.</summary>
    public static void PrintLine(UInt128 value) => Write(value, newline: true);

    // ---- floats -------------------------------------------------------------------

    /// <summary>Writes an <c>f32</c> as Rust's <c>Display</c> would.</summary>
    public static void Print(float value) => Write(Format.Single(value), newline: false);
    /// <summary>Writes an <c>f32</c> and a newline.</summary>
    public static void PrintLine(float value) => Write(Format.Single(value), newline: true);

    /// <summary>Writes an <c>f64</c> as Rust's <c>Display</c> would.</summary>
    public static void Print(double value) => Write(Format.Double(value), newline: false);
    /// <summary>Writes an <c>f64</c> and a newline.</summary>
    public static void PrintLine(double value) => Write(Format.Double(value), newline: true);

    // ---- bool, char, strings ------------------------------------------------------

    /// <summary>Writes <c>true</c> or <c>false</c>.</summary>
    public static void Print(bool value) => Write(value ? "true" : "false", newline: false);
    /// <summary>Writes <c>true</c> or <c>false</c> and a newline.</summary>
    public static void PrintLine(bool value) => Write(value ? "true" : "false", newline: true);

    /// <summary>Writes one UTF-16 code unit.</summary>
    public static void Print(char value) => Out.Write(value);
    /// <summary>Writes one UTF-16 code unit and a newline.</summary>
    public static void PrintLine(char value)
    {
        char[] buffer = Buffer;
        buffer[0] = value;
        buffer[1] = '\n';
        Out.Write(buffer, 0, 2);
    }

    /// <summary>Writes a string (<c>null</c> writes nothing).</summary>
    public static void Print(string? value) => Write(value, newline: false);
    /// <summary>Writes a string and a newline.</summary>
    public static void PrintLine(string? value) => Write(value, newline: true);

    /// <summary>Writes a <c>&amp;str</c>.</summary>
    public static void Print(ReadOnlySpan<char> value) => Write(value, newline: false);
    /// <summary>Writes a <c>&amp;str</c> and a newline.</summary>
    public static void PrintLine(ReadOnlySpan<char> value) => Write(value, newline: true);

    // ---- helpers ------------------------------------------------------------------

    /// <summary>Formats a number with the invariant culture into the thread buffer and writes it once.</summary>
    private static void Write<T>(T value, bool newline) where T : struct, ISpanFormattable
    {
        char[] buffer = Buffer;
        if (value.TryFormat(buffer, out int n, default, CultureInfo.InvariantCulture))
        {
            if (newline) buffer[n++] = '\n';
            Out.Write(buffer, 0, n);
        }
        else
        {
            Write(value.ToString(null, CultureInfo.InvariantCulture), newline);
        }
    }

    private static void Write(string? value, bool newline) => Write(value.AsSpan(), newline);

    /// <summary>Writes text and an optional newline, in one write when it fits the thread buffer.</summary>
    private static void Write(ReadOnlySpan<char> value, bool newline)
    {
        if (!newline)
        {
            Out.Write(value);
            return;
        }
        char[] buffer = Buffer;
        if (value.Length < buffer.Length)
        {
            value.CopyTo(buffer);
            buffer[value.Length] = '\n';
            Out.Write(buffer, 0, value.Length + 1);
        }
        else
        {
            Out.Write(value);
            Out.Write('\n');
        }
    }
}
