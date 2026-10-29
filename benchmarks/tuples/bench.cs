// Tuples in a hot loop, with `System.ValueTuple`; see bench.sabi.
using System;

static class Program
{
    const ulong Modulus = 1_000_000_007;
    const ulong Rounds = 20_000_000;

    static (ulong, ulong) Step((ulong, ulong) state)
    {
        var (a, b) = state;
        return (b, (a + b) % Modulus);
    }

    static (ulong, ulong) DivMod(ulong a, ulong b) => (a / b, a % b);

    static void Main()
    {
        var fib = (0UL, 1UL);
        var acc = (0UL, 0UL, 0UL);
        for (ulong i = 1; i <= Rounds; i++)
        {
            fib = Step(fib);
            var (q, r) = DivMod(fib.Item2, i % 97 + 1);
            acc = (acc.Item1 + q % 1000, acc.Item2 ^ r, acc.Item3 + 1);
        }
        Console.WriteLine(fib.Item1);
        Console.WriteLine(acc.Item1);
        Console.WriteLine(acc.Item2);
        Console.WriteLine(acc.Item3);
    }
}
