using System;
using System.Runtime.CompilerServices;

namespace Sabiko.Runtime;

/// <summary>
/// Integer operations generated code needs and the base class library does not provide
/// with Rust's semantics. <c>i128</c>/<c>u128</c> are <see cref="Int128"/>/<see cref="UInt128"/>
/// (decision 42); most of their operations are the BCL's operator methods, called directly.
/// </summary>
public static class Arithmetic
{
    // ---- overflow detection for debug builds (decision 43) ---------------------------
    //
    // Debug builds check `+`, `-`, `*` and unary `-` on 64- and 128-bit integers with these
    // helpers: each stores the wrapped result in `result` and returns whether the exact
    // result overflowed, and the generated code raises the panic itself, with the
    // operator's message and the source location. (Narrower types are checked inline, by
    // computing in a wider type.) `isize`/`usize` are 64-bit (decision 18). They are small
    // enough for the JIT to inline.

    /// <summary><c>a + b</c> for <c>i64</c>; true when it overflows.</summary>
    [MethodImpl(MethodImplOptions.AggressiveInlining)]
    public static bool AddOverflows(long a, long b, out long result)
    {
        result = unchecked(a + b);
        return ((a ^ result) & (b ^ result)) < 0;
    }

    /// <summary><c>a - b</c> for <c>i64</c>; true when it overflows.</summary>
    [MethodImpl(MethodImplOptions.AggressiveInlining)]
    public static bool SubOverflows(long a, long b, out long result)
    {
        result = unchecked(a - b);
        return ((a ^ b) & (a ^ result)) < 0;
    }

    /// <summary><c>a * b</c> for <c>i64</c>; true when it overflows.</summary>
    [MethodImpl(MethodImplOptions.AggressiveInlining)]
    public static bool MulOverflows(long a, long b, out long result)
    {
        long high = Math.BigMul(a, b, out result);
        return high != (result >> 63);
    }

    /// <summary><c>a + b</c> for <c>u64</c>; true when it overflows.</summary>
    [MethodImpl(MethodImplOptions.AggressiveInlining)]
    public static bool AddOverflows(ulong a, ulong b, out ulong result)
    {
        result = unchecked(a + b);
        return result < a;
    }

    /// <summary><c>a - b</c> for <c>u64</c>; true when it overflows.</summary>
    [MethodImpl(MethodImplOptions.AggressiveInlining)]
    public static bool SubOverflows(ulong a, ulong b, out ulong result)
    {
        result = unchecked(a - b);
        return a < b;
    }

    /// <summary><c>a * b</c> for <c>u64</c>; true when it overflows.</summary>
    [MethodImpl(MethodImplOptions.AggressiveInlining)]
    public static bool MulOverflows(ulong a, ulong b, out ulong result)
    {
        return Math.BigMul(a, b, out result) != 0;
    }

    /// <summary><c>a + b</c> for <c>isize</c>; true when it overflows.</summary>
    [MethodImpl(MethodImplOptions.AggressiveInlining)]
    public static bool AddOverflows(nint a, nint b, out nint result)
    {
        bool overflows = AddOverflows((long)a, (long)b, out long wide);
        result = (nint)wide;
        return overflows;
    }

    /// <summary><c>a - b</c> for <c>isize</c>; true when it overflows.</summary>
    [MethodImpl(MethodImplOptions.AggressiveInlining)]
    public static bool SubOverflows(nint a, nint b, out nint result)
    {
        bool overflows = SubOverflows((long)a, (long)b, out long wide);
        result = (nint)wide;
        return overflows;
    }

    /// <summary><c>a * b</c> for <c>isize</c>; true when it overflows.</summary>
    [MethodImpl(MethodImplOptions.AggressiveInlining)]
    public static bool MulOverflows(nint a, nint b, out nint result)
    {
        bool overflows = MulOverflows((long)a, (long)b, out long wide);
        result = (nint)wide;
        return overflows;
    }

    /// <summary><c>a + b</c> for <c>usize</c>; true when it overflows.</summary>
    [MethodImpl(MethodImplOptions.AggressiveInlining)]
    public static bool AddOverflows(nuint a, nuint b, out nuint result)
    {
        bool overflows = AddOverflows((ulong)a, (ulong)b, out ulong wide);
        result = (nuint)wide;
        return overflows;
    }

    /// <summary><c>a - b</c> for <c>usize</c>; true when it overflows.</summary>
    [MethodImpl(MethodImplOptions.AggressiveInlining)]
    public static bool SubOverflows(nuint a, nuint b, out nuint result)
    {
        bool overflows = SubOverflows((ulong)a, (ulong)b, out ulong wide);
        result = (nuint)wide;
        return overflows;
    }

    /// <summary><c>a * b</c> for <c>usize</c>; true when it overflows.</summary>
    [MethodImpl(MethodImplOptions.AggressiveInlining)]
    public static bool MulOverflows(nuint a, nuint b, out nuint result)
    {
        bool overflows = MulOverflows((ulong)a, (ulong)b, out ulong wide);
        result = (nuint)wide;
        return overflows;
    }

    /// <summary><c>a + b</c> for <c>i128</c>; true when it overflows.</summary>
    [MethodImpl(MethodImplOptions.AggressiveInlining)]
    public static bool AddOverflows(Int128 a, Int128 b, out Int128 result)
    {
        result = unchecked(a + b);
        return Int128.IsNegative((a ^ result) & (b ^ result));
    }

    /// <summary><c>a - b</c> for <c>i128</c>; true when it overflows.</summary>
    [MethodImpl(MethodImplOptions.AggressiveInlining)]
    public static bool SubOverflows(Int128 a, Int128 b, out Int128 result)
    {
        result = unchecked(a - b);
        return Int128.IsNegative((a ^ b) & (a ^ result));
    }

    /// <summary>
    /// <c>a * b</c> for <c>i128</c>; true when it overflows: the product of the magnitudes
    /// must fit in 127 bits, or be exactly 2^127 when the result is negative.
    /// </summary>
    public static bool MulOverflows(Int128 a, Int128 b, out Int128 result)
    {
        bool negative = Int128.IsNegative(a) != Int128.IsNegative(b);
        bool overflows = MulOverflows(Magnitude(a), Magnitude(b), out UInt128 product);
        // The wrapped product is the wrapped magnitude product with the sign applied.
        result = negative ? unchecked(-(Int128)product) : (Int128)product;
        UInt128 limit = negative ? (UInt128)Int128.MaxValue + UInt128.One : (UInt128)Int128.MaxValue;
        return overflows || product > limit;
    }

    /// <summary><c>a + b</c> for <c>u128</c>; true when it overflows.</summary>
    [MethodImpl(MethodImplOptions.AggressiveInlining)]
    public static bool AddOverflows(UInt128 a, UInt128 b, out UInt128 result)
    {
        result = unchecked(a + b);
        return result < a;
    }

    /// <summary><c>a - b</c> for <c>u128</c>; true when it overflows.</summary>
    [MethodImpl(MethodImplOptions.AggressiveInlining)]
    public static bool SubOverflows(UInt128 a, UInt128 b, out UInt128 result)
    {
        result = unchecked(a - b);
        return a < b;
    }

    /// <summary>
    /// <c>a * b</c> for <c>u128</c>; true when it overflows. With both operands split into
    /// 64-bit halves, the product fits only if at most one operand has a high half, that
    /// half times the other's low half fits in 64 bits, and adding it (shifted) to the
    /// product of the low halves does not carry out.
    /// </summary>
    public static bool MulOverflows(UInt128 a, UInt128 b, out UInt128 result)
    {
        result = unchecked(a * b);
        ulong aHigh = (ulong)(a >> 64);
        ulong bHigh = (ulong)(b >> 64);
        if (aHigh != 0 && bHigh != 0)
        {
            return true;
        }
        ulong other = aHigh != 0 ? (ulong)b : (ulong)a;
        if (Math.BigMul(aHigh | bHigh, other, out ulong cross) != 0)
        {
            return true;
        }
        UInt128 low = (UInt128)(ulong)a * (ulong)b;
        UInt128 sum = unchecked(low + ((UInt128)cross << 64));
        return sum < low;
    }

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
    private static UInt128 Magnitude(Int128 value) =>
        Int128.IsNegative(value) ? (UInt128)unchecked(-value) : (UInt128)value;

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
