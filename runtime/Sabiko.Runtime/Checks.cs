using System.Diagnostics.CodeAnalysis;
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
    /// <c>internal error: entered unreachable code</c>, without a location: code generation
    /// emits it where control cannot arrive (after a call that never returns, and for MIR's
    /// <c>unreachable</c>).
    /// </summary>
    [DoesNotReturn]
    [MethodImpl(MethodImplOptions.NoInlining)]
    public static void Unreachable() =>
        throw new PanicException("internal error: entered unreachable code");
}
