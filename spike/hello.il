// Phase 0 toolchain spike: hand-written CIL exercising the shapes Sabiko will emit.
//   fn AddOne(val: &mut i32)   -> static void AddOne(int32& val)
//   struct Point { X: i32, Y: i32 } -> sequential valuetype
//   fn MovePoint(p: &mut Point, dx: i32, dy: i32)
.assembly extern System.Runtime { .ver 10:0:0:0 .publickeytoken = (B0 3F 5F 7F 11 D5 0A 3A) }
.assembly extern System.Console { .ver 10:0:0:0 .publickeytoken = (B0 3F 5F 7F 11 D5 0A 3A) }
.assembly Hello { .ver 0:1:0:0 }
.module Hello.exe

.class public sequential ansi sealed beforefieldinit Point extends [System.Runtime]System.ValueType
{
  .field public int32 X
  .field public int32 Y
}

.class public abstract auto ansi sealed beforefieldinit Program extends [System.Runtime]System.Object
{
  .method public hidebysig static void AddOne(int32& val) cil managed
  {
    .maxstack 3
    ldarg.0
    ldarg.0
    ldind.i4
    ldc.i4.1
    add
    stind.i4
    ret
  }

  .method public hidebysig static void MovePoint(valuetype Point& p, int32 dx, int32 dy) cil managed
  {
    .maxstack 3
    ldarg.0
    ldarg.0
    ldfld int32 Point::X
    ldarg.1
    add
    stfld int32 Point::X
    ldarg.0
    ldarg.0
    ldfld int32 Point::Y
    ldarg.2
    add
    stfld int32 Point::Y
    ret
  }

  .method public hidebysig static void Main() cil managed
  {
    .entrypoint
    .maxstack 3
    .locals init (
      [0] int32 count,
      [1] valuetype Point cursor
    )
    // let mut count = 5; AddOne(&mut count);
    ldc.i4.5
    stloc.0
    ldloca.s 0
    call void Program::AddOne(int32&)
    ldloc.0
    call void [System.Console]System.Console::WriteLine(int32)

    // let mut cursor = Point { X: 10, Y: 10 }; MovePoint(&mut cursor, 5, -2);
    ldloca.s 1
    initobj Point
    ldloca.s 1
    ldc.i4.s 10
    stfld int32 Point::X
    ldloca.s 1
    ldc.i4.s 10
    stfld int32 Point::Y
    ldloca.s 1
    ldc.i4.5
    ldc.i4.s -2
    call void Program::MovePoint(valuetype Point&, int32, int32)
    ldloca.s 1
    ldfld int32 Point::X
    call void [System.Console]System.Console::WriteLine(int32)
    ldloca.s 1
    ldfld int32 Point::Y
    call void [System.Console]System.Console::WriteLine(int32)
    ldstr "Hello from Sabiko's toolchain spike"
    call void [System.Console]System.Console::WriteLine(string)
    ret
  }
}
