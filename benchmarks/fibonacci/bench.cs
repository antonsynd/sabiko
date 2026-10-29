// Fibonacci, recursively and iteratively; see bench.sabi.
using System;

static class Program
{
    const long Modulus = 1_000_000_007;

    static long FibRecursive(long n)
    {
        if (n <= 1) return n;
        return FibRecursive(n - 1) + FibRecursive(n - 2);
    }

    static long FibIterative(long n)
    {
        if (n <= 1) return n;
        long a = 0, b = 1;
        for (long i = 2; i <= n; i++)
        {
            long next = a + b;
            a = b;
            b = next;
        }
        return b;
    }

    static void Main()
    {
        Console.WriteLine(FibRecursive(32));
        long sum = 0;
        for (long i = 0; i < 300_000; i++)
        {
            sum = (sum + FibIterative(i % 90) % Modulus) % Modulus;
        }
        Console.WriteLine(sum);
    }
}
