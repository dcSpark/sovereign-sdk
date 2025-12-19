(module
  (type (;0;) (func (param i32 i32) (result i32)))
  (type (;1;) (func (param i32)))
  (type (;2;) (func))
  (import "wasi_snapshot_preview1" "args_sizes_get" (func (;0;) (type 0)))
  (import "wasi_snapshot_preview1" "args_get" (func (;1;) (type 0)))
  (import "wasi_snapshot_preview1" "proc_exit" (func (;2;) (type 1)))
  (import "env" "assert_one" (func (;3;) (type 1)))
  (func (;4;) (type 2)
    (local i32 i32 i32)
    global.get 0
    i32.const 176
    i32.sub
    local.tee 0
    global.set 0
    local.get 0
    i32.const 0
    i32.store offset=8
    local.get 0
    i32.const 0
    i32.store offset=12
    block  ;; label = @1
      block  ;; label = @2
        local.get 0
        i32.const 8
        i32.add
        local.get 0
        i32.const 12
        i32.add
        call 0
        br_if 0 (;@2;)
        block  ;; label = @3
          local.get 0
          i32.load offset=8
          i32.const 8
          i32.gt_u
          br_if 0 (;@3;)
          local.get 0
          i32.load offset=12
          i32.const 128
          i32.gt_u
          br_if 0 (;@3;)
          i32.const 0
          local.set 1
          block  ;; label = @4
            loop  ;; label = @5
              local.get 1
              i32.const 32
              i32.eq
              br_if 1 (;@4;)
              local.get 0
              i32.const 16
              i32.add
              local.get 1
              i32.add
              i32.const 0
              i32.store
              local.get 1
              i32.const 4
              i32.add
              local.set 1
              br 0 (;@5;)
            end
          end
          block  ;; label = @4
            i32.const 128
            i32.eqz
            br_if 0 (;@4;)
            local.get 0
            i32.const 48
            i32.add
            i32.const 0
            i32.const 128
            memory.fill
          end
          local.get 0
          i32.const 16
          i32.add
          local.get 0
          i32.const 48
          i32.add
          call 1
          i32.eqz
          br_if 2 (;@1;)
          i32.const 71
          call 2
          unreachable
        end
        i32.const 71
        call 2
        unreachable
      end
      i32.const 71
      call 2
      unreachable
    end
    local.get 0
    i32.load offset=24
    i32.load align=1
    local.set 2
    local.get 0
    i32.load offset=20
    i32.load align=1
    local.tee 1
    i32.const -1
    i32.xor
    i32.const 31
    i32.shr_u
    call 3
    local.get 1
    i32.const 65536
    i32.lt_s
    call 3
    local.get 1
    local.get 2
    i32.eq
    call 3
    i32.const 0
    call 2
    unreachable)
  (memory (;0;) 16)
  (global (;0;) (mut i32) (i32.const 1048576))
  (export "memory" (memory 0))
  (export "_start" (func 4)))
