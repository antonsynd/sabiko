// The C# half of the `enum_layout` interop test (see lib.sabi): reads the layout of
// Sabiko's enums by reflection, builds values by writing their tag and payload fields,
// calls Sabiko functions by value, `in` (`&E`) and `ref` (`&mut E`), reads the tags and
// payloads of what they return, and checks that a `switch` on the tags agrees with
// Sabiko's `match`. Every check prints its result; a mismatch also exits with 1.
// Built by crates/sabikoc/tests/interop.rs against lib.dll and Sabiko.Runtime.dll.

using System;
using System.Collections.Generic;
using System.Globalization;
using System.Linq;
using System.Reflection;

// The discriminants Sabiko assigns, written out by hand: the test is that they agree.
static class ColorTag { public const int Red = 0, Green = 1, Blue = 2; }
static class CodeTag { public const long Low = -2, Mid = -1, High = 300, Huge = 5000000000; }
static class TokenTag { public const int Tag = 0, Value = 1; }
static class ShapeTag { public const int Circle = 0, Rect = 1, Line = 2, Empty = 3; }
static class EventTag { public const int Click = 0, Key = 1, Draw = 2, Quit = 3; }

static class Test
{
    static int failures;

    static void Check<T>(string what, T expected, T actual)
    {
        bool ok = EqualityComparer<T>.Default.Equals(expected, actual);
        Console.WriteLine(ok ? $"{what}: {actual}" : $"{what}: {actual} MISMATCH (expected {expected})");
        if (!ok) failures++;
    }

    // ----- building values field by field ---------------------------------------------

    static Point P(int x, int y) => new Point { X = x, Y = y };
    static Code NewCode(long tag) => new Code { Tag = tag };
    static Color NewColor(int tag) => new Color { Tag = tag };
    static Shape Circle(double r) => new Shape { Tag = ShapeTag.Circle, Circle = new Shape.CirclePayload { Item1 = r } };
    static Shape Rect(int w, int h) => new Shape { Tag = ShapeTag.Rect, Rect = new Shape.RectPayload { W = w, H = h } };
    static Shape Line(Point a, Point b) => new Shape { Tag = ShapeTag.Line, Line = new Shape.LinePayload { Item1 = a, Item2 = b } };
    static Shape Empty() => new Shape { Tag = ShapeTag.Empty };
    static Event Click(int x, int y) => new Event { Tag = EventTag.Click, Click = new Event.ClickPayload { Item1 = P(x, y) } };
    static Event Quit() => new Event { Tag = EventTag.Quit };

    static Event Key(long code, bool shift)
    {
        // Payload fields written in place, as `e.Key.Code` and `e.Key.Shift`.
        var e = new Event();
        e.Tag = EventTag.Key;
        e.Key.Code.Tag = code;
        e.Key.Shift = shift;
        return e;
    }

    static Event Draw(Shape s, int color)
    {
        var e = new Event { Tag = EventTag.Draw };
        e.Draw.Item1 = s;
        e.Draw.Item2.Tag = color;
        return e;
    }

    // ----- the same logic as lib.sabi, as switches on the tags --------------------------

    static int Classify(in Event e) => e.Tag switch
    {
        EventTag.Click => e.Click.Item1 switch
        {
            { X: 0, Y: 0 } => 1,
            { X: < 0 } => 2,
            _ => 3,
        },
        EventTag.Key => (e.Key.Code.Tag, e.Key.Shift) switch
        {
            (CodeTag.Huge, true) => 4,
            (CodeTag.Low or CodeTag.Mid, _) => 5,
            _ => 6,
        },
        EventTag.Draw => (e.Draw.Item1.Tag, e.Draw.Item2.Tag) switch
        {
            (ShapeTag.Empty, _) => 7,
            (ShapeTag.Rect, ColorTag.Red) when e.Draw.Item1.Rect.W == e.Draw.Item1.Rect.H => 8,
            (_, ColorTag.Blue) => 9,
            _ => 10,
        },
        EventTag.Quit => 0,
        _ => throw new InvalidOperationException($"bad Event tag {e.Tag}"),
    };

    static double Area(in Shape s) => s.Tag switch
    {
        ShapeTag.Circle => 3.0 * s.Circle.Item1 * s.Circle.Item1,
        ShapeTag.Rect => s.Rect.W * s.Rect.H,
        _ => 0.0,
    };

    static int Kind(Shape s) => (s.Tag + 1) * 10;

    static string Show(in Event e) => e.Tag switch
    {
        EventTag.Click => $"click {e.Click.Item1.X} {e.Click.Item1.Y}",
        EventTag.Key => $"key {e.Key.Code.Tag} shift {(e.Key.Shift ? "true" : "false")}",
        EventTag.Draw => $"draw kind {Kind(e.Draw.Item1)} area {Area(e.Draw.Item1)} color {e.Draw.Item2.Tag}",
        EventTag.Quit => "quit",
        _ => throw new InvalidOperationException($"bad Event tag {e.Tag}"),
    };

    static string Describe(in Event e) => $"{Show(e)} [{Classify(e)}]";

    // ----- the layout, by reflection --------------------------------------------------

    static string Layout(Type t)
    {
        var fields = t.GetFields(BindingFlags.Public | BindingFlags.Instance)
            .OrderBy(f => f.MetadataToken)
            .Select(f => $"{f.Name}: {f.FieldType.FullName}");
        string kind = t.IsValueType && t.IsLayoutSequential ? "sequential struct" : "?";
        return $"{t.FullName} ({kind}) {{ {string.Join(", ", fields)} }}";
    }

    static int Main()
    {
        CultureInfo.CurrentCulture = CultureInfo.InvariantCulture;

        Console.WriteLine("== layout");
        foreach (var t in new[] { typeof(Color), typeof(Code), typeof(Token), typeof(Wrapper), typeof(Point), typeof(Shape), typeof(Event) })
        {
            Console.WriteLine(Layout(t));
            foreach (var nested in t.GetNestedTypes().OrderBy(n => n.MetadataToken))
            {
                Console.WriteLine("  " + Layout(nested));
            }
        }

        Console.WriteLine("== fieldless enums");
        Check("Code::Low as i64", CodeTag.Low, Code.Value(NewCode(CodeTag.Low)));
        Check("Code::Huge as i64", CodeTag.Huge, Code.Value(NewCode(CodeTag.Huge)));
        Code c = NewCode(CodeTag.Low);
        foreach (long expected in new[] { CodeTag.Mid, CodeTag.High, CodeTag.Huge, CodeTag.Low })
        {
            c = Code.Next(c);
            Check("Code::Next tag", expected, c.Tag);
        }
        for (int i = 0; i < 4; i++)
        {
            Check($"ColorOf({i}) tag", Math.Min(i, ColorTag.Blue), Lib.ColorOf(i).Tag);
        }

        Console.WriteLine("== a variant called Tag");
        var token = new Token { Tag_ = TokenTag.Value, Value = new Token.ValuePayload { Item1 = 4 } };
        Check("TokenValue(Value(4))", 4, Lib.TokenValue(token));
        Check("TokenValue(Tag)", -1, Lib.TokenValue(new Token { Tag_ = TokenTag.Tag }));
        Check("CountDown(&mut Value(4))", 10, Lib.CountDown(ref token));
        Check("token tag after CountDown", TokenTag.Tag, token.Tag_);

        Console.WriteLine("== a variant named like a payload type");
        Check("Unwrap(Item(7))", 7, Lib.Unwrap(new Wrapper { Tag = 0, Item = new Wrapper.ItemPayload_ { Item1 = 7 } }));
        Check("Unwrap(ItemPayload(true))", 1,
            Lib.Unwrap(new Wrapper { Tag = 1, ItemPayload = new Wrapper.ItemPayloadPayload { Item1 = true } }));

        Console.WriteLine("== shapes");
        var shapes = new[] { Circle(1.5), Rect(3, 4), Line(P(1, 1), P(2, 3)), Empty(), Shape.Square(5), Lib.MakeLine(0, 0, 4, -2) };
        foreach (var s0 in shapes)
        {
            var s = s0;
            Check($"tag {s.Tag} Area", Area(s), Shape.Area(in s));
            Check($"tag {s.Tag} Kind", Kind(s), Shape.Kind(s));
            Shape.Scale(ref s, 2);
            Check($"tag {s.Tag} unchanged by Scale", s0.Tag, s.Tag);
            switch (s.Tag)
            {
                case ShapeTag.Circle: Check("scaled radius", s0.Circle.Item1 * 2, s.Circle.Item1); break;
                case ShapeTag.Rect: Check("scaled rect", (s0.Rect.W * 2, s0.Rect.H * 2), (s.Rect.W, s.Rect.H)); break;
                case ShapeTag.Line:
                    Check("line start kept", (s0.Line.Item1.X, s0.Line.Item1.Y), (s.Line.Item1.X, s.Line.Item1.Y));
                    Check("line end x", s0.Line.Item1.X + (s0.Line.Item2.X - s0.Line.Item1.X) * 2, s.Line.Item2.X);
                    Check("line end y", s0.Line.Item1.Y + (s0.Line.Item2.Y - s0.Line.Item1.Y) * 2, s.Line.Item2.Y);
                    break;
            }
        }
        Shape square = Shape.Square(7);
        Check("Square(7) payload", (ShapeTag.Rect, 7, 7), (square.Tag, square.Rect.W, square.Rect.H));

        Console.WriteLine("== events");
        var events = new List<Event>
        {
            Click(0, 0), Click(-3, 8), Click(2, 2),
            Key(CodeTag.Huge, true), Key(CodeTag.Huge, false), Key(CodeTag.Mid, true), Key(CodeTag.High, true),
            Draw(Empty(), ColorTag.Blue), Draw(Rect(2, 2), ColorTag.Red), Draw(Rect(2, 3), ColorTag.Red),
            Draw(Circle(0.5), ColorTag.Blue), Draw(Circle(2), ColorTag.Green),
            Quit(),
            Lib.MakeKey(NewCode(CodeTag.Low), false), Lib.MakeDraw(Shape.Square(3), NewColor(ColorTag.Red)),
            default,
        };
        foreach (var e0 in events)
        {
            var e = e0;
            Console.WriteLine("c#:     " + Describe(e));
            Console.Write("sabiko: ");
            Lib.Show(in e);
            Check("  Classify", Classify(e), Lib.Classify(in e));
            Check("  DrawnColor", e.Tag == EventTag.Draw ? e.Draw.Item2.Tag : -1, Lib.DrawnColor(in e));
            Check("  KeyCode", e.Tag == EventTag.Key ? e.Key.Code.Tag : 0, Lib.KeyCode(e));
            Lib.Move(ref e, 10, 20);
            Console.WriteLine("  moved: " + Describe(e));
        }

        Console.WriteLine("== reading what Sabiko built");
        Event k = Lib.MakeKey(NewCode(CodeTag.High), true);
        Check("MakeKey payload", (EventTag.Key, CodeTag.High, true), (k.Tag, k.Key.Code.Tag, k.Key.Shift));
        Event q = Quit();
        Lib.Move(ref q, -1, -2);
        Check("Quit moved becomes Click", (EventTag.Click, -1, -2), (q.Tag, q.Click.Item1.X, q.Click.Item1.Y));
        Event d = Lib.MakeDraw(Lib.MakeLine(1, 2, 3, 4), NewColor(ColorTag.Green));
        Lib.Move(ref d, 1, 1);
        Check("moved line", (ShapeTag.Line, 2, 3, 4, 5),
            (d.Draw.Item1.Tag, d.Draw.Item1.Line.Item1.X, d.Draw.Item1.Line.Item1.Y, d.Draw.Item1.Line.Item2.X, d.Draw.Item1.Line.Item2.Y));

        Console.WriteLine("== a struct by value, in and ref");
        Point a = P(2, 3), b = P(4, -5);
        Check("Dot", a.X * b.X + a.Y * b.Y, Lib.Dot(a, in b));
        Lib.Mirror(ref b);
        Check("Mirror", (-5, 4), (b.X, b.Y));

        Console.WriteLine("== a tag no variant has");
        Check("default(Code) as i64", 0L, Code.Value(default));
        try
        {
            Code.Next(default);
            Check("Code::Next(default) panics", true, false);
        }
        catch (Sabiko.Runtime.PanicException ex)
        {
            Check("Code::Next(default) panics", "internal error: entered unreachable code", ex.Message);
        }
        try
        {
            var bad = new Shape { Tag = 9 };
            Shape.Area(in bad);
            Check("Area(tag 9) panics", true, false);
        }
        catch (Sabiko.Runtime.PanicException ex)
        {
            Check("Area(tag 9) panics", "internal error: entered unreachable code", ex.Message);
        }

        Console.WriteLine(failures == 0 ? "all checks passed" : $"{failures} checks failed");
        return failures == 0 ? 0 : 1;
    }
}
