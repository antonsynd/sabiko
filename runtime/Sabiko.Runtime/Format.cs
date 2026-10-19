using System;
using System.Globalization;

namespace Sabiko.Runtime;

/// <summary>
/// Formats floating-point values exactly as Rust's <c>Display</c> does: the shortest
/// digit string that round-trips, never in exponent notation, no fractional part for
/// integral values, <c>-0</c> for negative zero, <c>NaN</c>, <c>inf</c> and <c>-inf</c>.
/// </summary>
internal static class Format
{
    /// <summary>Formats an <c>f64</c>.</summary>
    internal static string Double(double value)
    {
        if (double.IsNaN(value)) return "NaN";
        if (double.IsPositiveInfinity(value)) return "inf";
        if (double.IsNegativeInfinity(value)) return "-inf";
        // "R" on .NET Core 3.0+ is the shortest round-trippable representation, possibly in
        // exponent notation ("1E+21", "1E-07"), which Expand rewrites to plain decimal.
        return Expand(value.ToString("R", CultureInfo.InvariantCulture));
    }

    /// <summary>Formats an <c>f32</c> using float's own shortest round-trip digits.</summary>
    internal static string Single(float value)
    {
        if (float.IsNaN(value)) return "NaN";
        if (float.IsPositiveInfinity(value)) return "inf";
        if (float.IsNegativeInfinity(value)) return "-inf";
        return Expand(value.ToString("R", CultureInfo.InvariantCulture));
    }

    /// <summary>
    /// Rewrites a round-trip string such as <c>-1.25E+21</c>, <c>5E-324</c>, <c>0.1</c> or
    /// <c>-0</c> into plain positional decimal by moving the decimal point along the digit
    /// string and padding with zeros: shortest digits then zeros, as Rust prints, not the
    /// exact binary expansion.
    /// </summary>
    internal static string Expand(string roundTrip)
    {
        ReadOnlySpan<char> s = roundTrip;
        bool negative = false;
        if (s.Length > 0 && s[0] == '-')
        {
            negative = true;
            s = s[1..];
        }

        int exponent = 0;
        int e = s.IndexOfAny('E', 'e');
        if (e >= 0)
        {
            exponent = int.Parse(s[(e + 1)..], NumberStyles.AllowLeadingSign, CultureInfo.InvariantCulture);
            s = s[..e];
        }

        // Digits without the point; pointPos is the position of the decimal point within them.
        int dot = s.IndexOf('.');
        string digits;
        int pointPos;
        if (dot < 0)
        {
            digits = s.ToString();
            pointPos = s.Length;
        }
        else
        {
            digits = string.Concat(s[..dot], s[(dot + 1)..]);
            pointPos = dot;
        }
        pointPos += exponent;

        // Normalise: drop leading zeros (moving the point) and trailing zeros (no-ops).
        int lead = 0;
        while (lead < digits.Length && digits[lead] == '0') lead++;
        int end = digits.Length;
        while (end > lead && digits[end - 1] == '0') end--;
        digits = digits[lead..end];
        pointPos -= lead;

        if (digits.Length == 0)
        {
            return negative ? "-0" : "0";
        }

        string body;
        if (pointPos <= 0)
        {
            body = string.Concat("0.", new string('0', -pointPos), digits);
        }
        else if (pointPos >= digits.Length)
        {
            body = string.Concat(digits, new string('0', pointPos - digits.Length));
        }
        else
        {
            body = string.Concat(digits.AsSpan(0, pointPos), ".", digits.AsSpan(pointPos));
        }
        return negative ? string.Concat("-", body) : body;
    }
}
