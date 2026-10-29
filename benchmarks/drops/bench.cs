// Values with drop glue in a hot loop, with explicit cleanup calls; see bench.sabi.
using System;

struct Guard
{
    public ulong Id, Weight;

    public Guard(ulong id, ulong weight)
    {
        Id = id;
        Weight = weight;
    }

    public readonly void Drop()
    {
        if (Id % 1_000_000 == 0)
        {
            Console.Write("dropped ");
            Console.WriteLine(Weight);
        }
    }
}

struct Pair
{
    public Guard A, B;
    public ulong Tag;
}

static class Program
{
    const ulong Rounds = 10_000_000;

    static Guard Make(ulong id, ulong seed) => new Guard(id, seed % 1000);

    static ulong Consume(Guard g)
    {
        try
        {
            return g.Weight + 1;
        }
        finally
        {
            g.Drop();
        }
    }

    static void Main()
    {
        ulong total = 0;
        var keep = Make(0, 1);
        try
        {
            for (ulong i = 1; i < Rounds; i++)
            {
                var p = new Pair { A = Make(i, i * 7), B = Make(i + Rounds, i * 13), Tag = i };
                bool aMoved = false;
                try
                {
                    if (i % 3 == 0)
                    {
                        aMoved = true;
                        total += Consume(p.A);
                    }
                    else
                    {
                        total += p.A.Weight;
                    }
                    total += p.B.Weight + p.Tag % 7;
                }
                finally
                {
                    if (!aMoved)
                    {
                        p.A.Drop();
                    }
                    p.B.Drop();
                }
                if (i % 5 == 0)
                {
                    var old = keep;
                    keep = Make(i, total);
                    old.Drop();
                }
            }
            total += keep.Weight;
            Console.WriteLine(total);
        }
        finally
        {
            keep.Drop();
        }
    }
}
