using System.Diagnostics.CodeAnalysis;
using System.Globalization;
using System.Runtime.CompilerServices;

namespace Sabiko.Runtime;

/// <summary>
/// The failing side of the run-time checks generated code performs (decision 43): each
/// method raises a <see cref="PanicException"/> with Rust's message for one check, located
/// at <c>location</c> (<c>file:line:column</c>), and never returns.
/// </summary>
/// <remarks>
/// A failed check in generated code is <c>ldstr location; call Checks::X(string); ldnull;
/// throw</c>. The <c>throw</c> after the call is never reached; it ends the block, so the
/// verifier sees no fall-through and the JIT treats the block as cold, moving it out of the
/// hot path. Keeping the message, the <c>newobj</c> and the <c>throw</c> here, out of line
/// (<see cref="MethodImplOptions.NoInlining"/>), keeps every checked operation small, which
/// matters to the JIT's inlining heuristics: they are driven by IL size. Each check passes
/// one string, since the message is implied by the method.
/// </remarks>
public static class Checks
{
    /// <summary><c>attempt to add with overflow</c>.</summary>
    [DoesNotReturn]
    [MethodImpl(MethodImplOptions.NoInlining)]
    public static void AddOverflow(string location) =>
        throw new PanicException("attempt to add with overflow", location);

    /// <summary><c>attempt to subtract with overflow</c>.</summary>
    [DoesNotReturn]
    [MethodImpl(MethodImplOptions.NoInlining)]
    public static void SubtractOverflow(string location) =>
        throw new PanicException("attempt to subtract with overflow", location);

    /// <summary><c>attempt to multiply with overflow</c>.</summary>
    [DoesNotReturn]
    [MethodImpl(MethodImplOptions.NoInlining)]
    public static void MultiplyOverflow(string location) =>
        throw new PanicException("attempt to multiply with overflow", location);

    /// <summary><c>attempt to negate with overflow</c>.</summary>
    [DoesNotReturn]
    [MethodImpl(MethodImplOptions.NoInlining)]
    public static void NegateOverflow(string location) =>
        throw new PanicException("attempt to negate with overflow", location);

    /// <summary><c>attempt to divide by zero</c>.</summary>
    [DoesNotReturn]
    [MethodImpl(MethodImplOptions.NoInlining)]
    public static void DivideByZero(string location) =>
        throw new PanicException("attempt to divide by zero", location);

    /// <summary><c>attempt to divide with overflow</c> (<c>MIN / -1</c>).</summary>
    [DoesNotReturn]
    [MethodImpl(MethodImplOptions.NoInlining)]
    public static void DivideOverflow(string location) =>
        throw new PanicException("attempt to divide with overflow", location);

    /// <summary><c>attempt to calculate the remainder with a divisor of zero</c>.</summary>
    [DoesNotReturn]
    [MethodImpl(MethodImplOptions.NoInlining)]
    public static void RemainderByZero(string location) =>
        throw new PanicException("attempt to calculate the remainder with a divisor of zero", location);

    /// <summary><c>attempt to calculate the remainder with overflow</c> (<c>MIN % -1</c>).</summary>
    [DoesNotReturn]
    [MethodImpl(MethodImplOptions.NoInlining)]
    public static void RemainderOverflow(string location) =>
        throw new PanicException("attempt to calculate the remainder with overflow", location);

    /// <summary><c>attempt to shift left with overflow</c>.</summary>
    [DoesNotReturn]
    [MethodImpl(MethodImplOptions.NoInlining)]
    public static void ShiftLeftOverflow(string location) =>
        throw new PanicException("attempt to shift left with overflow", location);

    /// <summary><c>attempt to shift right with overflow</c>.</summary>
    [DoesNotReturn]
    [MethodImpl(MethodImplOptions.NoInlining)]
    public static void ShiftRightOverflow(string location) =>
        throw new PanicException("attempt to shift right with overflow", location);

    /// <summary>
    /// <c>index out of bounds: the len is {len} but the index is {index}</c>: an array or
    /// slice element read, written or borrowed at an index past its end. Generated code
    /// passes the length and the index (both <c>usize</c>) before the location.
    /// </summary>
    [DoesNotReturn]
    [MethodImpl(MethodImplOptions.NoInlining)]
    public static void IndexOutOfBounds(nuint len, nuint index, string location) =>
        throw new PanicException(
            string.Concat(
                "index out of bounds: the len is ",
                len.ToString(CultureInfo.InvariantCulture),
                " but the index is ",
                index.ToString(CultureInfo.InvariantCulture)),
            location);

    /// <summary>
    /// A string sliced out of range (#10): <c>s[start..end]</c>, or with <c>inclusive</c>
    /// <c>s[start..=end]</c>. Rust's checks run in this order and its messages say
    /// "byte" because Rust strings are UTF-8; Sabiko's indices count UTF-16 code units
    /// (decision 62), and the messages keep Rust's words so the two reports match.
    /// </summary>
    [DoesNotReturn]
    [MethodImpl(MethodImplOptions.NoInlining)]
    public static void StrSliceFail(nuint start, nuint end, bool inclusive, nuint len, string location)
    {
        string l = len.ToString(CultureInfo.InvariantCulture);
        if (start > len)
            throw new PanicException(
                string.Concat("start byte index ", Show(start), " is out of bounds for string of length ", l),
                location);
        if (inclusive ? end >= len : end > len)
            throw new PanicException(
                string.Concat("end byte index ", Show(end), " is out of bounds for string of length ", l),
                location);
        throw new PanicException(
            string.Concat("byte range starts at ", Show(start), " but ends at ", Show(inclusive ? end + 1 : end)),
            location);
    }

    /// <summary>
    /// An array or slice sliced out of range (#9, #10): <c>a[start..end]</c>, or with
    /// <c>inclusive</c> <c>a[start..=end]</c>, with Rust's checks and messages.
    /// </summary>
    [DoesNotReturn]
    [MethodImpl(MethodImplOptions.NoInlining)]
    public static void SliceFail(nuint start, nuint end, bool inclusive, nuint len, string location)
    {
        string l = len.ToString(CultureInfo.InvariantCulture);
        if (start > len)
            throw new PanicException(
                string.Concat("range start index ", Show(start), " out of range for slice of length ", l),
                location);
        if (inclusive ? end >= len : end > len)
            throw new PanicException(
                string.Concat("range end index ", Show(end), " out of range for slice of length ", l),
                location);
        throw new PanicException(
            string.Concat("slice index starts at ", Show(start), " but ends at ", Show(inclusive ? end + 1 : end)),
            location);
    }

    private static string Show(nuint value) => value.ToString(CultureInfo.InvariantCulture);

    /// <summary>
    /// <c>internal error: entered unreachable code</c>, without a location: code generation
    /// emits it where control cannot arrive (after a call that never returns, and for MIR's
    /// <c>unreachable</c>).
    /// </summary>
    [DoesNotReturn]
    [MethodImpl(MethodImplOptions.NoInlining)]
    public static void Unreachable() =>
        throw new PanicException("internal error: entered unreachable code");
}
