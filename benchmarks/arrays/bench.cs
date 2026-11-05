// Arrays and slices in hot loops, with heap arrays indexed through `Span<T>`; see
// bench.sabi.
using System;

static class Program
{
    const int SieveSize = 200_000;
    const uint SieveRounds = 40;
    const int SortSize = 2_000;
    const uint SortRounds = 12;
    const int Dim = 64;
    const uint MatrixRounds = 40;

    static uint Sieve(Span<bool> composite)
    {
        int n = composite.Length;
        for (int i = 0; i < n; i++)
            composite[i] = false;
        uint count = 0;
        for (int p = 2; p < n; p++)
        {
            if (!composite[p])
            {
                count++;
                for (long m = (long)p * p; m < n; m += p)
                    composite[(int)m] = true;
            }
        }
        return count;
    }

    static ulong Fill(Span<int> values, ulong seed)
    {
        ulong state = seed;
        for (int i = 0; i < values.Length; i++)
        {
            state = state * 48271 % 2147483647;
            values[i] = (int)(state % 100_000);
        }
        return state;
    }

    static void InsertionSort(Span<int> values)
    {
        for (int i = 1; i < values.Length; i++)
        {
            int key = values[i];
            int j = i;
            while (j > 0 && values[j - 1] > key)
            {
                values[j] = values[j - 1];
                j--;
            }
            values[j] = key;
        }
    }

    static void Multiply(double[,] a, double[,] b, double[,] c)
    {
        for (int i = 0; i < Dim; i++)
        {
            for (int j = 0; j < Dim; j++)
            {
                double sum = 0.0;
                for (int k = 0; k < Dim; k++)
                    sum += a[i, k] * b[k, j];
                c[i, j] = sum;
            }
        }
    }

    static void Main()
    {
        var composite = new bool[SieveSize];
        uint primes = 0;
        for (uint round = 0; round < SieveRounds; round++)
            primes += Sieve(composite);
        Console.WriteLine(primes);

        var values = new int[SortSize];
        ulong seed = 12345;
        long checksum = 0;
        for (uint round = 0; round < SortRounds; round++)
        {
            seed = Fill(values, seed);
            InsertionSort(values);
            checksum += (long)values[0] + values[SortSize / 2] + values[SortSize - 1];
        }
        Console.WriteLine(checksum);

        var a = new double[Dim, Dim];
        var b = new double[Dim, Dim];
        var c = new double[Dim, Dim];
        for (int i = 0; i < Dim; i++)
        {
            for (int j = 0; j < Dim; j++)
            {
                a[i, j] = (double)(i + j) / Dim;
                b[i, j] = ((double)i - j) / Dim;
            }
        }
        double trace = 0.0;
        for (uint round = 0; round < MatrixRounds; round++)
        {
            Multiply(a, b, c);
            a[round % Dim, 0] += 1.0;
            trace += c[round % Dim, round % Dim];
        }
        Console.WriteLine(trace);
    }
}
