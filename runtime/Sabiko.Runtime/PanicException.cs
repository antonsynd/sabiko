using System;
namespace Sabiko.Runtime;

/// <summary>
/// The exception a Sabiko <c>panic!</c> is lowered to. Sabiko programs do not catch it;
/// it unwinds through <c>try/finally</c> blocks (so <c>Drop</c> runs) to the entry-point
/// wrapper, which hands it to <see cref="Panic.Unhandled"/>. C# callers may catch it.
/// </summary>
public sealed class PanicException : Exception
{
    /// <summary>Creates a panic carrying <paramref name="message"/>.</summary>
    public PanicException(string message) : base(message)
    {
    }
}
