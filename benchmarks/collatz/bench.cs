// The longest Collatz chain starting below a limit; see bench.sabi.
using System;

static class Program
{
    const ulong Limit = 300_000;

    static uint ChainLength(ulong start)
    {
        ulong n = start;
        uint steps = 1;
        while (n != 1)
        {
            if (n % 2 == 0)
                n = n / 2;
            else
                n = 3 * n + 1;
            steps++;
        }
        return steps;
    }

    static void Main()
    {
        ulong best = 1;
        uint bestLength = 1;
        for (ulong i = 1; i < Limit; i++)
        {
            uint length = ChainLength(i);
            if (length > bestLength)
            {
                bestLength = length;
                best = i;
            }
        }
        Console.WriteLine(best);
        Console.WriteLine(bestLength);
    }
}
