using System;

namespace Sabiko.Runtime;

/// <summary>
/// Sabiko's <c>()</c> where it is stored as a value: a parameter, a struct field or a tuple
/// element (decision 52). It has exactly one value and no state, so generated code never
/// reads or writes one; it only passes <c>default(Unit)</c> where the IL needs a value.
/// A function returning <c>()</c> is <c>void</c>, and a local of type <c>()</c> has no
/// storage at all.
/// </summary>
public readonly struct Unit : IEquatable<Unit>
{
    /// <summary>Every <c>()</c> equals every other.</summary>
    public bool Equals(Unit other) => true;

    /// <inheritdoc/>
    public override bool Equals(object? obj) => obj is Unit;

    /// <inheritdoc/>
    public override int GetHashCode() => 0;

    /// <summary>Rust's <c>Debug</c> rendering of <c>()</c>.</summary>
    public override string ToString() => "()";

    /// <summary>Always <see langword="true"/>.</summary>
    public static bool operator ==(Unit left, Unit right) => true;

    /// <summary>Always <see langword="false"/>.</summary>
    public static bool operator !=(Unit left, Unit right) => false;
}
