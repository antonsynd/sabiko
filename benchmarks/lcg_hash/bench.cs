// A Lehmer (MINSTD) random number generator feeding a polynomial hash; see bench.sabi.
using System;

static class Program
{
    const ulong Modulus = 2_147_483_647;
    const ulong Multiplier = 48_271;
    const ulong HashModulus = 1_000_000_007;
    const uint Rounds = 30_000_000;

    static ulong Next(ulong state) => state * Multiplier % Modulus;

    static void Main()
    {
        ulong state = 1, hash = 0;
        for (uint i = 0; i < Rounds; i++)
        {
            state = Next(state);
            hash = (hash * 31 + (state & 255)) % HashModulus;
        }
        Console.WriteLine(state);
        Console.WriteLine(hash);
    }
}
