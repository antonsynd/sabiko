using System;
using System.Runtime.CompilerServices;

namespace Sabiko.Runtime;

/// <summary>
/// Operations on <c>&amp;str</c> (<see cref="ReadOnlySpan{T}"/> of <see cref="char"/>,
/// decision 7) that generated code calls (#10).
/// </summary>
public static class Str
{
    /// <summary><c>a == b</c>: the same UTF-16 code units.</summary>
    [MethodImpl(MethodImplOptions.AggressiveInlining)]
    public static bool Equal(ReadOnlySpan<char> a, ReadOnlySpan<char> b) => a.SequenceEqual(b);

    /// <summary>
    /// The order of <c>a</c> and <c>b</c>, negative, zero or positive: lexicographic by
    /// UTF-16 code unit, which is Rust's order (by UTF-8 byte, which is by Unicode scalar
    /// value) except between a character above U+FFFF (a surrogate pair) and one from
    /// U+E000 to U+FFFF (decision 62).
    /// </summary>
    [MethodImpl(MethodImplOptions.AggressiveInlining)]
    public static int Compare(ReadOnlySpan<char> a, ReadOnlySpan<char> b) => a.SequenceCompareTo(b);
}
