(module
  (type (;0;) (func))
  (type (;1;) (func (param i32)))
  (type (;2;) (func (param i32 i32) (result i32)))
  (type (;3;) (func (result i32)))
  (import "env" "assert_one" (func (;0;) (type 1)))
  (import "wasi_snapshot_preview1" "args_sizes_get" (func (;1;) (type 2)))
  (import "wasi_snapshot_preview1" "args_get" (func (;2;) (type 2)))
  (import "wasi_snapshot_preview1" "proc_exit" (func (;3;) (type 1)))
  (func (;4;) (type 0)
    call 12)
  (func (;5;) (type 2) (param i32 i32) (result i32)
    (local i32 i32)
    global.get 0
    i32.const 32
    i32.sub
    local.set 2
    local.get 2
    global.set 0
    local.get 2
    i32.const 0
    i32.store offset=28
    local.get 2
    local.get 0
    i32.store offset=24
    local.get 2
    local.get 1
    i32.store offset=20
    local.get 2
    local.get 2
    i32.load offset=20
    i32.load offset=4
    i32.load
    i32.store offset=16
    local.get 2
    local.get 2
    i32.load offset=20
    i32.load offset=8
    i32.load
    i32.store offset=12
    local.get 2
    i32.load offset=16
    i32.const 0
    i32.ge_s
    i32.const 1
    i32.and
    call 0
    local.get 2
    i32.load offset=16
    i32.const 65535
    i32.le_s
    i32.const 1
    i32.and
    call 0
    local.get 2
    i32.load offset=16
    local.get 2
    i32.load offset=12
    i32.eq
    i32.const 1
    i32.and
    call 0
    i32.const 0
    local.set 3
    local.get 2
    i32.const 32
    i32.add
    global.set 0
    local.get 3
    return)
  (func (;6;) (type 0)
    block  ;; label = @1
      i32.const 1
      i32.eqz
      br_if 0 (;@1;)
      call 4
    end
    call 7
    call 10
    unreachable)
  (func (;7;) (type 3) (result i32)
    (local i32 i32 i32 i32)
    global.get 0
    i32.const 16
    i32.sub
    local.tee 0
    global.set 0
    block  ;; label = @1
      block  ;; label = @2
        local.get 0
        local.tee 1
        i32.const 12
        i32.add
        local.get 1
        i32.const 8
        i32.add
        call 1
        br_if 0 (;@2;)
        block  ;; label = @3
          block  ;; label = @4
            local.get 1
            i32.load offset=12
            local.tee 2
            br_if 0 (;@4;)
            i32.const 0
            local.set 2
            i32.const 0
            local.set 0
            br 1 (;@3;)
          end
          local.get 0
          local.get 2
          i32.const 2
          i32.shl
          local.tee 2
          i32.const 19
          i32.add
          i32.const -16
          i32.and
          i32.sub
          local.tee 0
          local.tee 3
          global.set 0
          local.get 3
          local.get 1
          i32.load offset=8
          i32.const 15
          i32.add
          i32.const -16
          i32.and
          i32.sub
          local.tee 3
          global.set 0
          local.get 0
          local.get 2
          i32.add
          i32.const 0
          i32.store
          local.get 0
          local.get 3
          call 2
          br_if 2 (;@1;)
          local.get 1
          i32.load offset=12
          local.set 2
        end
        local.get 2
        local.get 0
        call 5
        local.set 0
        local.get 1
        i32.const 16
        i32.add
        global.set 0
        local.get 0
        return
      end
      i32.const 71
      call 3
      unreachable
    end
    i32.const 71
    call 3
    unreachable)
  (func (;8;) (type 0))
  (func (;9;) (type 0)
    (local i32)
    i32.const 0
    local.set 0
    block  ;; label = @1
      i32.const 0
      i32.const 0
      i32.le_u
      br_if 0 (;@1;)
      loop  ;; label = @2
        local.get 0
        i32.const -4
        i32.add
        local.tee 0
        i32.load
        call_indirect (type 0)
        local.get 0
        i32.const 0
        i32.gt_u
        br_if 0 (;@2;)
      end
    end
    call 8)
  (func (;10;) (type 1) (param i32)
    call 8
    call 9
    call 8
    local.get 0
    call 11
    unreachable)
  (func (;11;) (type 1) (param i32)
    local.get 0
    call 3
    unreachable)
  (func (;12;) (type 0)
    i32.const 65536
    global.set 2
    i32.const 0
    i32.const 15
    i32.add
    i32.const -16
    i32.and
    global.set 1)
  (func (;13;) (type 3) (result i32)
    global.get 0
    global.get 1
    i32.sub)
  (func (;14;) (type 3) (result i32)
    global.get 2)
  (func (;15;) (type 3) (result i32)
    global.get 1)
  (func (;16;) (type 1) (param i32)
    local.get 0
    global.set 0)
  (func (;17;) (type 3) (result i32)
    global.get 0)
  (table (;0;) 2 2 funcref)
  (memory (;0;) 257 257)
  (global (;0;) (mut i32) (i32.const 65536))
  (global (;1;) (mut i32) (i32.const 0))
  (global (;2;) (mut i32) (i32.const 0))
  (export "memory" (memory 0))
  (export "_start" (func 6))
  (export "__indirect_function_table" (table 0))
  (export "emscripten_stack_init" (func 12))
  (export "emscripten_stack_get_free" (func 13))
  (export "emscripten_stack_get_base" (func 14))
  (export "emscripten_stack_get_end" (func 15))
  (export "_emscripten_stack_restore" (func 16))
  (export "emscripten_stack_get_current" (func 17))
  (elem (;0;) (i32.const 1) func 4))
