using System;

namespace Sabiko.Runtime;

/// <summary>
/// Integer operations generated code needs and the base class library does not provide
/// with Rust's semantics. <c>i128</c>/<c>u128</c> are <see cref="Int128"/>/<see cref="UInt128"/>
/// (decision 42); most of their operations are the BCL's operator methods, called directly.
/// </summary>
public static class Arithmetic
{
    // ---- 128-bit integer to float (`as f32`, `as f64`) ------------------------------

    /// <summary>
    /// <c>i128 as f32</c>: rounded once, to nearest with ties to even, as Rust does. The BCL
    /// conversion goes through <c>double</c> and can round twice
    /// (2^53 + 2^29 + 1 gives 2^53 instead of 2^53 + 2^30).
    /// </summary>
    public static float ToSingle(Int128 value) =>
        Int128.IsNegative(value) ? -ToSingle(Magnitude(value)) : ToSingle((UInt128)value);

    /// <summary><c>u128 as f32</c>, rounded once (see <see cref="ToSingle(Int128)"/>).</summary>
    public static float ToSingle(UInt128 value)
    {
        (long bits, int scale) = Reduce(value);
        // `long` to `float` is one rounding (`conv.r4` from `int64`); scaling by a power of
        // two is exact, or overflows to infinity as Rust's conversion does.
        return float.ScaleB((float)bits, scale);
    }

    /// <summary><c>i128 as f64</c>, rounded once to nearest, ties to even.</summary>
    public static double ToDouble(Int128 value) =>
        Int128.IsNegative(value) ? -ToDouble(Magnitude(value)) : ToDouble((UInt128)value);

    /// <summary><c>u128 as f64</c>, rounded once to nearest, ties to even.</summary>
    public static double ToDouble(UInt128 value)
    {
        (long bits, int scale) = Reduce(value);
        return double.ScaleB((double)bits, scale);
    }

    /// <summary>|value| as an unsigned value; <c>MinValue</c> gives 2^127.</summary>
    private static UInt128 Magnitude(Int128 value) => (UInt128)(-value);

    /// <summary>
    /// Splits <paramref name="value"/> into a non-negative 62-bit integer and a power of two
    /// such that <c>bits * 2^scale</c> rounds to the same float as <paramref name="value"/>
    /// in every format with at most 60 significant bits (both <c>float</c> and
    /// <c>double</c>): the bits shifted out are folded into the lowest bit (a "sticky" bit),
    /// which is all round-to-nearest needs to know about them. Small values are returned
    /// unchanged with scale 0.
    /// </summary>
    private static (long Bits, int Scale) Reduce(UInt128 value)
    {
        const int Width = 62;
        int length = 128 - (int)UInt128.LeadingZeroCount(value);
        if (length <= Width)
        {
            return ((long)(ulong)value, 0);
        }
        int scale = length - Width;
        ulong bits = (ulong)(value >> scale);
        if ((value & ((UInt128.One << scale) - UInt128.One)) != UInt128.Zero)
        {
            bits |= 1;
        }
        return ((long)bits, scale);
    }
}
