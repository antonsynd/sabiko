using System;
using System.Globalization;
using System.IO;
using System.Diagnostics.CodeAnalysis;
using System.Runtime.CompilerServices;

namespace Sabiko.Runtime;

/// <summary>Panic entry points called from generated code.</summary>
public static class Panic
{
    /// <summary>Rust's exit code for a process that died of a panic.</summary>
    public const int ExitCode = 101;

    /// <summary>Raises a panic with <paramref name="message"/> (<c>panic!("...")</c>).</summary>
    [DoesNotReturn]
    [MethodImpl(MethodImplOptions.NoInlining)]
    public static void Fail(string message)
    {
        throw new PanicException(message);
    }

    /// <summary>Raises a panic with a <c>&amp;str</c> message.</summary>
    [DoesNotReturn]
    [MethodImpl(MethodImplOptions.NoInlining)]
    public static void Fail(ReadOnlySpan<char> message)
    {
        throw new PanicException(message.ToString());
    }

    /// <summary>Raises a panic with <paramref name="message"/> at <paramref name="location"/>.</summary>
    [DoesNotReturn]
    [MethodImpl(MethodImplOptions.NoInlining)]
    public static void Fail(string message, string location)
    {
        throw new PanicException(message, location);
    }

    /// <summary>
    /// Raises a panic with a <c>&amp;str</c> message at <paramref name="location"/>
    /// (<c>file:line:column</c>): Sabiko's <c>Panic(msg)</c>.
    /// </summary>
    [DoesNotReturn]
    [MethodImpl(MethodImplOptions.NoInlining)]
    public static void Fail(ReadOnlySpan<char> message, string location)
    {
        throw new PanicException(message.ToString(), location);
    }

    // ---- FailDebug: `panic!("{msg}: {value:?}")` ----------------------------------------
    //
    // The `core` prelude's `PanicDebug` intrinsic (decision 90), which `Result::unwrap`,
    // `expect`, `unwrap_err` and `expect_err` call: the message, `": "`, then the value as
    // Rust's `Debug` formats it, which for an integer or a `bool` is what `Print` writes.

    /// <summary>Panics with <c>"{message}: {value:?}"</c> for an <c>i8</c>.</summary>
    [DoesNotReturn]
    [MethodImpl(MethodImplOptions.NoInlining)]
    public static void FailDebug(ReadOnlySpan<char> message, sbyte value, string? location) =>
        FailDebug(message, value.ToString(CultureInfo.InvariantCulture), location);

    /// <summary>Panics with <c>"{message}: {value:?}"</c> for an <c>i16</c>.</summary>
    [DoesNotReturn]
    [MethodImpl(MethodImplOptions.NoInlining)]
    public static void FailDebug(ReadOnlySpan<char> message, short value, string? location) =>
        FailDebug(message, value.ToString(CultureInfo.InvariantCulture), location);

    /// <summary>Panics with <c>"{message}: {value:?}"</c> for an <c>i32</c>.</summary>
    [DoesNotReturn]
    [MethodImpl(MethodImplOptions.NoInlining)]
    public static void FailDebug(ReadOnlySpan<char> message, int value, string? location) =>
        FailDebug(message, value.ToString(CultureInfo.InvariantCulture), location);

    /// <summary>Panics with <c>"{message}: {value:?}"</c> for an <c>i64</c>.</summary>
    [DoesNotReturn]
    [MethodImpl(MethodImplOptions.NoInlining)]
    public static void FailDebug(ReadOnlySpan<char> message, long value, string? location) =>
        FailDebug(message, value.ToString(CultureInfo.InvariantCulture), location);

    /// <summary>Panics with <c>"{message}: {value:?}"</c> for an <c>isize</c>.</summary>
    [DoesNotReturn]
    [MethodImpl(MethodImplOptions.NoInlining)]
    public static void FailDebug(ReadOnlySpan<char> message, nint value, string? location) =>
        FailDebug(message, value.ToString(CultureInfo.InvariantCulture), location);

    /// <summary>Panics with <c>"{message}: {value:?}"</c> for an <c>i128</c>.</summary>
    [DoesNotReturn]
    [MethodImpl(MethodImplOptions.NoInlining)]
    public static void FailDebug(ReadOnlySpan<char> message, Int128 value, string? location) =>
        FailDebug(message, value.ToString(CultureInfo.InvariantCulture), location);

    /// <summary>Panics with <c>"{message}: {value:?}"</c> for a <c>u8</c>.</summary>
    [DoesNotReturn]
    [MethodImpl(MethodImplOptions.NoInlining)]
    public static void FailDebug(ReadOnlySpan<char> message, byte value, string? location) =>
        FailDebug(message, value.ToString(CultureInfo.InvariantCulture), location);

    /// <summary>Panics with <c>"{message}: {value:?}"</c> for a <c>u16</c>.</summary>
    [DoesNotReturn]
    [MethodImpl(MethodImplOptions.NoInlining)]
    public static void FailDebug(ReadOnlySpan<char> message, ushort value, string? location) =>
        FailDebug(message, value.ToString(CultureInfo.InvariantCulture), location);

    /// <summary>Panics with <c>"{message}: {value:?}"</c> for a <c>u32</c>.</summary>
    [DoesNotReturn]
    [MethodImpl(MethodImplOptions.NoInlining)]
    public static void FailDebug(ReadOnlySpan<char> message, uint value, string? location) =>
        FailDebug(message, value.ToString(CultureInfo.InvariantCulture), location);

    /// <summary>Panics with <c>"{message}: {value:?}"</c> for a <c>u64</c>.</summary>
    [DoesNotReturn]
    [MethodImpl(MethodImplOptions.NoInlining)]
    public static void FailDebug(ReadOnlySpan<char> message, ulong value, string? location) =>
        FailDebug(message, value.ToString(CultureInfo.InvariantCulture), location);

    /// <summary>Panics with <c>"{message}: {value:?}"</c> for a <c>usize</c>.</summary>
    [DoesNotReturn]
    [MethodImpl(MethodImplOptions.NoInlining)]
    public static void FailDebug(ReadOnlySpan<char> message, nuint value, string? location) =>
        FailDebug(message, value.ToString(CultureInfo.InvariantCulture), location);

    /// <summary>Panics with <c>"{message}: {value:?}"</c> for a <c>u128</c>.</summary>
    [DoesNotReturn]
    [MethodImpl(MethodImplOptions.NoInlining)]
    public static void FailDebug(ReadOnlySpan<char> message, UInt128 value, string? location) =>
        FailDebug(message, value.ToString(CultureInfo.InvariantCulture), location);

    /// <summary>Panics with <c>"{message}: {value:?}"</c> for a <c>bool</c>.</summary>
    [DoesNotReturn]
    [MethodImpl(MethodImplOptions.NoInlining)]
    public static void FailDebug(ReadOnlySpan<char> message, bool value, string? location) =>
        FailDebug(message, value ? "true" : "false", location);

    /// <summary>
    /// Panics with <c>"{message}: {debug}"</c>, where <paramref name="debug"/> is the
    /// value's <c>Debug</c> text (<c>"()"</c> for <c>()</c>), at
    /// <paramref name="location"/> (<c>null</c> for none).
    /// </summary>
    [DoesNotReturn]
    [MethodImpl(MethodImplOptions.NoInlining)]
    public static void FailDebug(ReadOnlySpan<char> message, string debug, string? location)
    {
        string text = string.Concat(message, ": ", debug);
        throw location is null ? new PanicException(text) : new PanicException(text, location);
    }

    /// <summary>
    /// Called by the generated entry-point wrapper when an exception escapes the user's
    /// <c>Main</c>. Writes Rust's report to stderr, <c>thread 'main' panicked at
    /// file:line:column:</c> (or <c>thread 'main' panicked:</c> when the panic has no
    /// location), a newline, the panic message and a newline, and returns
    /// <see cref="ExitCode"/> (101) for the wrapper to return from <c>Main</c>.
    /// </summary>
    public static int Unhandled(Exception exception)
    {
        string message = MessageOf(exception);
        string? location = (exception as PanicException)?.Location;
        TextWriter err = System.Console.Error;
        if (location is null)
        {
            err.Write("thread 'main' panicked:\n");
        }
        else
        {
            err.Write("thread 'main' panicked at ");
            err.Write(location);
            err.Write(":\n");
        }
        err.Write(message);
        err.Write('\n');
        err.Flush();
        return ExitCode;
    }

    /// <summary>Set once a cleanup handler (a drop during unwinding) has started.</summary>
    [ThreadStatic]
    private static bool t_cleaningUp;

    /// <summary>The panic the entry point's filter accepted, for the abort report.</summary>
    [ThreadStatic]
    private static Exception? t_panic;

    /// <summary>
    /// Called first by every cleanup handler (a <c>fault</c> block that drops values while
    /// a panic unwinds, decision 55). Sabiko code cannot catch a panic, so from here on the
    /// thread is unwinding until the process ends: a panic raised now is a panic inside a
    /// destructor during cleanup, which <see cref="Filter"/> turns into an abort.
    /// </summary>
    public static void BeginCleanup()
    {
        t_cleaningUp = true;
    }

    /// <summary>
    /// The exception filter of the generated entry point. It runs in .NET's first pass,
    /// before any cleanup handler of the exception runs. For a panic raised while a cleanup
    /// handler runs, it aborts the process as rustc does ("panic in a destructor during
    /// cleanup"): it reports the panic being unwound and the new one, then fails fast
    /// (SIGABRT on Unix) without running any further drop. Otherwise it selects the
    /// handler (<see cref="Unhandled"/>), which runs once every cleanup handler has.
    /// </summary>
    public static int Filter(object exception)
    {
        if (t_cleaningUp)
        {
            Abort(exception);
        }
        t_panic = exception as Exception;
        return exception is Exception ? 1 : 0;
    }

    [DoesNotReturn]
    private static void Abort(object exception)
    {
        System.Console.Out.Flush();
        TextWriter err = System.Console.Error;
        if (t_panic is { } first)
        {
            Report(err, first);
            err.Write('\n');
        }
        if (exception is Exception second)
        {
            Report(err, second);
            err.Write('\n');
        }
        err.Write("thread 'main' panicked:\npanic in a destructor during cleanup\nthread caused non-unwinding panic. aborting.\n");
        err.Flush();
        Environment.FailFast("panic in a destructor during cleanup");
    }

    private static void Report(TextWriter err, Exception exception)
    {
        string? location = (exception as PanicException)?.Location;
        if (location is null)
        {
            err.Write("thread 'main' panicked:\n");
        }
        else
        {
            err.Write("thread 'main' panicked at ");
            err.Write(location);
            err.Write(":\n");
        }
        err.Write(MessageOf(exception));
        err.Write('\n');
    }

    /// <summary>
    /// Maps an escaping exception to the Rust panic message for it. Generated code raises
    /// only <see cref="PanicException"/>s (every check is explicit, decision 43); the other
    /// cases cover exceptions from the .NET code a program calls.
    /// </summary>
    internal static string MessageOf(Exception exception) => exception switch
    {
        PanicException panic => panic.Message,
        OverflowException => "attempt to perform arithmetic with overflow",
        DivideByZeroException => "attempt to divide by zero",
        ArithmeticException arithmetic => string.Concat("arithmetic error: ", arithmetic.Message),
        _ => string.Concat(exception.GetType().FullName, ": ", exception.Message),
    };
}
