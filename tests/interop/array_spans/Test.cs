// The C# half of the `array_spans` interop test (see lib.sabi): passes C# arrays and spans
// to Sabiko functions taking `&[i32]` (`ReadOnlySpan<int>`) and `&mut [i32]`
// (`Span<int>`), indexes the inline arrays Sabiko returns and holds in a struct, passes
// one by `ref` and `in`, reads a `ref readonly` element Sabiko returns, and catches the
// panic of an out-of-bounds index. Every check prints its result; a mismatch also exits
// with 1. Built by crates/sabikoc/tests/interop.rs against lib.dll and Sabiko.Runtime.dll.

using System;
using System.Collections.Generic;

static class Test
{
    static int failures;

    static void Check<T>(string what, T expected, T actual)
    {
        bool ok = EqualityComparer<T>.Default.Equals(expected, actual);
        Console.WriteLine(ok ? $"{what}: {actual}" : $"{what}: {actual} MISMATCH (expected {expected})");
        if (!ok) failures++;
    }

    static int Main()
    {
        int[] data = { 3, 1, 4, 1, 5 };
        Check("Sum(int[])", 14L, Lib.Sum(data));
        Check("Sum(slice of int[])", 6L, Lib.Sum(data.AsSpan(1, 3)));
        Check("Sum(empty)", 0L, Lib.Sum(ReadOnlySpan<int>.Empty));
        Span<int> stack = stackalloc int[] { 2, 7 };
        Lib.Scale(stack, 3);
        Check("Scale(stackalloc)", "6,21", $"{stack[0]},{stack[1]}");
        Lib.Scale(data, -1);
        Check("Scale(int[])", "-3,-1,-4,-1,-5", string.Join(",", data));
        Check("Largest(int[])", -1, Lib.Largest(data));
        ref readonly int largest = ref Lib.Largest(data);
        data[1] = -100;
        data[3] = 50;
        Check("Largest is a reference into the array", -100, largest);

        var made = Lib.Make(10);
        Check("Make(10)", "10,11,12,13", $"{made[0]},{made[1]},{made[2]},{made[3]}");
        Lib.Reverse(ref made);
        Check("Reverse(ref)", "13,12,11,10", $"{made[0]},{made[1]},{made[2]},{made[3]}");
        Check("Sum(AsSlice(in))", 46L, Lib.Sum(Lib.AsSlice(in made)));

        var samples = new Samples();
        samples.Values[0] = 5;
        samples.Values[1] = 6;
        samples.Values[2] = 7;
        samples.Values[3] = 100;
        samples.Count = 3;
        Check("Total(in Samples)", 18, Lib.Total(in samples));
        var copy = samples;
        copy.Values[0] = 0;
        Check("Samples is a value type", 5, samples.Values[0]);

        try
        {
            Lib.At(new[] { 1, 2, 3 }, 7);
            Check("At(7) panics", true, false);
        }
        catch (Sabiko.Runtime.PanicException e)
        {
            Check("At(7) panics", "index out of bounds: the len is 3 but the index is 7", e.Message);
        }
        return failures == 0 ? 0 : 1;
    }
}
