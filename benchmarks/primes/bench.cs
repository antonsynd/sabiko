// Counting primes by trial division; see bench.sabi.
using System;

static class Program
{
    const uint Limit = 600_000;

    static bool IsPrime(uint n)
    {
        if (n < 2) return false;
        if (n % 2 == 0) return n == 2;
        for (uint d = 3; d * d <= n; d += 2)
        {
            if (n % d == 0) return false;
        }
        return true;
    }

    static void Main()
    {
        uint count = 0, last = 0;
        for (uint n = 0; n < Limit; n++)
        {
            if (IsPrime(n))
            {
                count++;
                last = n;
            }
        }
        Console.WriteLine(count);
        Console.WriteLine(last);
    }
}
