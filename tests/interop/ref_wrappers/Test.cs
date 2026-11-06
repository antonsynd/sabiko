// The C# half of the `ref_wrappers` interop test (see lib.sabi): builds `Ref<T>` and
// `RefMut<T>` over C# locals (nested for `&&&i64`) and passes them `in` and `ref`. C#
// cannot read `Value` itself: a `ref` field whose type parameter `allows ref struct` is
// CS0570 ("not supported by the language"), which is what makes the wrappers nest. Every
// check prints its result; a mismatch also exits with 1. Built by
// crates/sabikoc/tests/interop.rs against lib.dll and Sabiko.Runtime.dll.

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
        int x = 5;
        var r = new Ref<int>(ref x);
        Check("Deref2", 5, Lib.Deref2(in r));
        x = 6;
        Check("the wrapper follows x", 6, Lib.Deref2(in r));

        long big = 1L << 40;
        var r1 = new Ref<long>(ref big);
        var r2 = new Ref<Ref<long>>(ref r1);
        Check("Deref3", 1L << 40, Lib.Deref3(in r2));

        int y = 1;
        var m = new RefMut<int>(ref y);
        Lib.Bump(ref m);
        Check("Bump through RefMut<int>", 11, y);

        return failures == 0 ? 0 : 1;
    }
}
