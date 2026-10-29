using System;
namespace Sabiko.Runtime;

/// <summary>
/// The exception a Sabiko <c>panic!</c> is lowered to. Sabiko programs do not catch it;
/// it unwinds through <c>try/finally</c> blocks (so <c>Drop</c> runs) to the entry-point
/// wrapper, which hands it to <see cref="Panic.Unhandled"/>. C# callers may catch it.
/// </summary>
public sealed class PanicException : Exception
{
    /// <summary>Creates a panic carrying <paramref name="message"/> and no source location.</summary>
    public PanicException(string message) : base(message)
    {
    }

    /// <summary>
    /// Creates a panic carrying <paramref name="message"/>, raised at
    /// <paramref name="location"/> (<c>file:line:column</c>, as Rust reports it).
    /// </summary>
    public PanicException(string message, string location) : base(message)
    {
        Location = location;
    }

    /// <summary>
    /// Where the panic was raised, as <c>file:line:column</c> (1-based; the column counts
    /// characters), or <c>null</c> when the panic site has no source position.
    /// </summary>
    public string? Location { get; }
}
