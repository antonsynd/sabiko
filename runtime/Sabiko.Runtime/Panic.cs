using System;
using System.IO;
using System.Diagnostics.CodeAnalysis;

namespace Sabiko.Runtime;

/// <summary>Panic entry points called from generated code.</summary>
public static class Panic
{
    /// <summary>Rust's exit code for a process that died of a panic.</summary>
    public const int ExitCode = 101;

    /// <summary>Raises a panic with <paramref name="message"/> (<c>panic!("...")</c>).</summary>
    [DoesNotReturn]
    public static void Fail(string message)
    {
        throw new PanicException(message);
    }

    /// <summary>Raises a panic with a <c>&amp;str</c> message.</summary>
    [DoesNotReturn]
    public static void Fail(ReadOnlySpan<char> message)
    {
        throw new PanicException(message.ToString());
    }

    /// <summary>Raises a panic with <paramref name="message"/> at <paramref name="location"/>.</summary>
    [DoesNotReturn]
    public static void Fail(string message, string location)
    {
        throw new PanicException(message, location);
    }

    /// <summary>
    /// Raises a panic with a <c>&amp;str</c> message at <paramref name="location"/>
    /// (<c>file:line:column</c>): Sabiko's <c>Panic(msg)</c>.
    /// </summary>
    [DoesNotReturn]
    public static void Fail(ReadOnlySpan<char> message, string location)
    {
        throw new PanicException(message.ToString(), location);
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
