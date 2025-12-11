# TEE Rollup Technical Specification (DRAFT)

## Table of Contents

1. [Overview](#1-overview)
   - 1.1 Scope and TEE Platform
   - 1.2 Key Differences from ZK Rollups
   - 1.3 Design Decisions
2. [Contract Architecture & Separation of Concerns](#2-contract-architecture--separation-of-concerns)
3. [System Architecture](#3-system-architecture)
4. [L1 Contract Storage](#4-l1-contract-storage)
5. [L1 Message Queue Storage](#5-l1-message-queue-storage)
6. [TEE Attestation Flow](#6-tee-attestation-flow)
   - 6.0 TEE Output = ZK Proof Output
   - 6.2.1 Canonical Hashing and report_data Binding
   - 6.2.2 Council Signature Digest (Domain Separation)
   - 6.4 Security Council Verification (On-chain vs Off-chain)
7. [Batch Lifecycle](#7-batch-lifecycle)
8. [L1 → L2 Deposits](#8-l1--l2-deposits)
   - 8.4.1 L1 Contract Message Queue Verification
9. [L2 → L1 Withdrawals](#9-l2--l1-withdrawals)
10. [Data Availability](#10-data-availability)
    - 10.2 DA Commitment Specification
    - 10.2.1 DA Commitment Algorithm
    - 10.2.2 DA Commitment Encoding Rules
11. [Security Model](#11-security-model)
    - 11.1.1 What Can Go Wrong
12. [Existing Codebase Components](#12-existing-codebase-components)

**Appendices**
- [Appendix A: Data Structures](#appendix-a-data-structures)
- [Appendix B: Gas Costs](#appendix-b-gas-costs)
- [Appendix C: Timing Parameters](#appendix-c-timing-parameters)
- [Appendix D: Contract Interfaces](#appendix-d-contract-interfaces)

---

## 1. Overview

This document specifies a Layer 2 rollup that uses **Trusted Execution Environments (TEE)** instead of ZK proofs to verify state transitions. The TEE enclave re-executes L2 blocks deterministically and produces cryptographic attestations that prove correct execution.

### 1.1 Scope and TEE Platform

**This specification is scoped to AMD SEV-SNP** running on Google Confidential VMs. While the architecture could be generalized to other TEE platforms (Intel SGX, Intel TDX, ARM TrustZone), this document specifies:

- **Hardware**: AMD SEV-SNP (Secure Encrypted Virtualization - Secure Nested Paging)
- **Platform**: Google Cloud Confidential VMs
- **Attestation**: AMD SEV-SNP attestation reports with Google certificate chain

Future versions may add support for additional TEE platforms with a generalized verifier interface.

### 1.2 Key Differences from ZK Rollups

| Aspect | ZK Rollup | TEE Rollup (this spec) |
|--------|-----------|------------------------|
| **Verification** | Mathematical proof | Hardware attestation + threshold signatures |
| **Trust assumption** | Math (trustless) | AMD hardware + Google infrastructure + Security Council |
| **Proof generation** | Minutes to hours | Seconds (real-time execution) |
| **On-chain verification** | Verifier contract parses proof | L1 verifies council signatures only |
| **Finality delay** | Proof generation time | Security Council verification latency |
| **Attestation verification** | On-chain | Off-chain (by Security Council) |

### 1.3 Design Decisions

This specification makes the following explicit design decisions:

| Decision | Choice | Rationale |
|----------|--------|-----------|
| **L2 block batching** | Multiple L2 blocks per batch | Amortize L1 costs (1 finalization per N blocks) |
| **Batch finalization** | One batch per `finalizeBatch` call | Simplicity; multi-batch bundling adds complexity |
| **TEE verification** | Off-chain only | Gas efficiency; council already trusted |
| **Message queue** | Index-based verification | Unambiguous progress tracking |
| **DA commitment** | Merkle tree of block IDs | Allows efficient verification |
| **Council signatures** | Domain-separated digest | Prevents replay attacks |

**Clarification on batching terminology:**
- **L2 blocks**: Produced every ~1-2 seconds by the sequencer
- **Batch**: A group of N consecutive L2 blocks processed together by the TEE
- **Finalization**: Submitting one batch's attestation to L1 via `finalizeBatch()`

```
L2 blocks:    [B1] [B2] [B3] [B4] [B5] [B6] [B7] [B8] [B9] [B10] ...
                └──────────────────┘     └───────────────────┘
                      Batch 1                 Batch 2
                         │                       │
                    TEE attests            TEE attests
                         │                       │
                    finalizeBatch(1)       finalizeBatch(2)
```

---

## 2. Contract Architecture & Separation of Concerns

This architecture follows a **single responsibility per contract** design philosophy. Each component handles one concern, making the system easier to audit, upgrade, and secure.

### 2.1 Why Separate Components?

```
┌─────────────────────────────────────────────────────────────────┐
│                   THE PROBLEM WITH MONOLITHS                    │
├─────────────────────────────────────────────────────────────────┤
│                                                                 │
│  ❌ One contract does everything:                               │
│                                                                 │
│  contract MonolithBridge {                                      │
│      function depositNIGHT() { ... }                            │
│      function depositToken() { ... }                            │
│      function depositNFT() { ... }                              │
│      function withdraw() { ... }                                │
│      function verifyProof() { ... }                             │
│      function queueMessage() { ... }                            │
│      function finalizeBatch() { ... }                           │
│      // 50+ more functions...                                   │
│  }                                                              │
│                                                                 │
│  Problems:                                                      │
│  • Huge attack surface - one bug affects everything             │
│  • Can't upgrade parts independently                            │
│  • Hits contract size limits                                    │
│  • Hard to audit - reviewers can't focus                        │
│  • Single point of failure                                      │
│                                                                 │
└─────────────────────────────────────────────────────────────────┘
```

### 2.2 The Layered Architecture

```
┌─────────────────────────────────────────────────────────────────┐
│                    CONTRACT RESPONSIBILITY LAYERS               │
├─────────────────────────────────────────────────────────────────┤
│                                                                 │
│  LAYER 1: GATEWAYS (Asset-Specific Logic)                       │
│  ─────────────────────────────────────────                      │
│  ┌──────────────┐ ┌──────────────┐ ┌──────────────┐             │
│  │ NIGHTGateway │ │ TokenGateway │ │ NFTGateway   │             │
│  │              │ │              │ │              │             │
│  │ • Lock NIGHT │ │ • Lock tokens│ │ • Lock NFTs  │             │
│  │ • Encode msg │ │ • Approvals  │ │ • Token IDs  │             │
│  └──────┬───────┘ └──────┬───────┘ └──────┬───────┘             │
│         │                │                │                     │
│         └────────────────┼────────────────┘                     │
│                          ▼                                      │
│  LAYER 2: MESSENGER (Cross-Domain Messaging)                    │
│  ─────────────────────────────────────────                      │
│  ┌──────────────────────────────────────────────────────────┐   │
│  │                    L1Messenger                           │   │
│  │                                                          │   │
│  │ • Message nonce assignment (prevent replay)              │   │
│  │ • Fee collection (pay for L2 gas)                        │   │
│  │ • NIGHT custody (all deposited NIGHT locked here)        │   │
│  │ • Message tracking (for replays/refunds)                 │   │
│  │ • Withdrawal proof verification                          │   │
│  └─────────────────────────┬────────────────────────────────┘   │
│                            ▼                                    │
│  LAYER 3: MESSAGE QUEUE (Ordered Storage)                       │
│  ─────────────────────────────────────────                      │
│  ┌──────────────────────────────────────────────────────────┐   │
│  │                    L1MessageQueue                        │   │
│  │                                                          │   │
│  │ • Rolling hash commitment (chain all messages)           │   │
│  │ • Timestamp tracking (for enforced mode)                 │   │
│  │ • Message indices (unique IDs)                           │   │
│  │ • Finalization tracking                                  │   │
│  └─────────────────────────┬────────────────────────────────┘   │
│                            ▼                                    │
│  LAYER 4: ROLLUP CHAIN (State Verification)                     │
│  ─────────────────────────────────────────                      │
│  ┌──────────────────────────────────────────────────────────┐   │
│  │                    RollupChain                           │   │
│  │                                                          │   │
│  │ • State root storage                                     │   │
│  │ • Withdraw root storage                                  │   │
│  │ • Batch hash storage                                     │   │
│  │ • TEE/ZK proof verification                              │   │
│  │ • Chain continuity checks                                │   │
│  └──────────────────────────────────────────────────────────┘   │
│                                                                 │
└─────────────────────────────────────────────────────────────────┘
```

### 2.3 Component Responsibilities

| Component | Single Responsibility | What It Does NOT Do |
|-----------|----------------------|---------------------|
| **Gateway** | Asset handling | ❌ Message ordering, ❌ Proof verification |
| **Messenger** | Cross-chain messaging | ❌ Asset logic, ❌ State storage |
| **MessageQueue** | Message commitment | ❌ Proof verification, ❌ Asset handling |
| **RollupChain** | State verification | ❌ Message handling, ❌ Asset logic |

### 2.4 Trust Boundaries

```
┌─────────────────────────────────────────────────────────────────┐
│                       TRUST BOUNDARIES                          │
├─────────────────────────────────────────────────────────────────┤
│                                                                 │
│  Gateway (NIGHT, Tokens, etc.)                                  │
│  ────────────────────────────                                   │
│  Trust: Only handles specific asset type                        │
│  Risk: Bug = lose tokens in THAT gateway only                   │
│  Upgrade: Can add new gateways without touching core            │
│                                                                 │
│  Messenger                                                      │
│  ────────────────────────────                                   │
│  Trust: Handles ALL cross-chain messages                        │
│  Risk: Bug = entire bridge compromised                          │
│  Security: Most audited, most critical                          │
│                                                                 │
│  MessageQueue                                                   │
│  ────────────────────────────                                   │
│  Trust: Determines what L2 MUST execute                         │
│  Risk: Bug = censorship or replay attacks                       │
│  Security: Simple logic, verifiable                             │
│                                                                 │
│  RollupChain                                                    │
│  ────────────────────────────                                   │
│  Trust: Validates L2 state transitions                          │
│  Risk: Bug = invalid state accepted                             │
│  Security: MOST CRITICAL - protects all user funds              │
│                                                                 │
└─────────────────────────────────────────────────────────────────┘
```

### 2.5 Data Flow Between Components

```
L1 → L2 DEPOSIT FLOW:
─────────────────────

User
 │
 │ depositNIGHT(amount)
 ▼
┌────────────────────┐
│    NIGHTGateway    │  ← LAYER 1: Asset Logic
│                    │
│ • Lock NIGHT       │
│ • Encode message   │
└─────────┬──────────┘
          │ sendMessage(to, value, data)
          ▼
┌────────────────────┐
│    Messenger       │  ← LAYER 2: Messaging
│                    │
│ • Assign nonce     │
│ • Collect fee      │
│ • Track message    │
└─────────┬──────────┘
          │ appendCrossDomainMessage()
          ▼
┌────────────────────┐
│   MessageQueue     │  ← LAYER 3: Storage
│                    │
│ • Compute hash     │
│ • Update rolling   │
│   hash chain       │
│ • Store timestamp  │
└─────────┬──────────┘
          │
          │ (L2 reads and executes)
          ▼
      L2 execution


L2 → L1 WITHDRAWAL FLOW:
────────────────────────

L2 User initiates withdrawal
          │
          │ (included in batch, TEE attests)
          ▼
┌────────────────────┐
│   RollupChain      │  ← LAYER 4: Verification
│                    │
│ • Verify TEE proof │
│ • Store stateRoot  │
│ • Store withdrawRoot│
└─────────┬──────────┘
          │
          │ (user claims with merkle proof)
          ▼
┌────────────────────┐
│    Messenger       │  ← LAYER 2: Proof Check
│                    │
│ • Check finalized  │
│ • Verify merkle    │
│   proof            │
│ • Execute message  │
└─────────┬──────────┘
          │
          ▼
┌────────────────────┐
│    NIGHTGateway    │  ← LAYER 1: Asset Release
│                    │
│ • Transfer NIGHT   │
│   to user          │
└────────────────────┘
```

### 2.6 Benefits of This Architecture

| Benefit | How It's Achieved |
|---------|-------------------|
| **Auditability** | Each contract is small and focused |
| **Upgradeability** | Can upgrade one layer without touching others |
| **Security isolation** | Bug in Gateway doesn't affect Messenger |
| **Extensibility** | Add new gateways (NFT, etc.) without core changes |
| **Gas efficiency** | Each contract optimized for its specific task |
| **Testing** | Can unit test each component in isolation |

### 2.7 Contract Interaction Matrix

```
                    │ Gateway │ Messenger │ MessageQueue │ RollupChain │
────────────────────┼─────────┼───────────┼──────────────┼─────────────┤
Gateway calls       │    -    │    ✓      │      ✗       │      ✗      │
Messenger calls     │    ✗    │    -      │      ✓       │      ✓      │
MessageQueue calls  │    ✗    │    ✗      │      -       │      ✗      │
RollupChain calls   │    ✗    │    ✗      │      ✓       │      -      │
User calls          │    ✓    │    ✓      │      ✗       │      ✗      │
Sequencer calls     │    ✗    │    ✗      │      ✗       │      ✓      │
Prover/TEE calls    │    ✗    │    ✗      │      ✗       │      ✓      │

✓ = calls directly
✗ = does not call (separation enforced)
```

### 2.8 Immutable vs Upgradeable Components

```
┌─────────────────────────────────────────────────────────────────┐
│                    UPGRADE STRATEGY                             │
├─────────────────────────────────────────────────────────────────┤
│                                                                 │
│  IMMUTABLE REFERENCES (set at deployment, never change):        │
│  ─────────────────────────────────────────────────────          │
│  • Gateway → Messenger address                                  │
│  • Messenger → MessageQueue address                             │
│  • Messenger → RollupChain address                              │
│  • RollupChain → MessageQueue address                           │
│                                                                 │
│  Why? Prevents upgrade attacks, ensures consistent behavior     │
│                                                                 │
│  UPGRADEABLE VIA PROXY:                                         │
│  ─────────────────────────────────────────────────────          │
│  • Gateway logic (add new asset features)                       │
│  • Messenger logic (fix bugs, add features)                     │
│  • RollupChain logic (change verification method)               │
│                                                                 │
│  Why? Allow improvements without redeploying everything         │
│                                                                 │
│  ADDING NEW COMPONENTS:                                         │
│  ─────────────────────────────────────────────────────          │
│  • New Gateways can be deployed anytime                         │
│  • They just need to call the existing Messenger                │
│  • No changes to core infrastructure needed                     │
│                                                                 │
└─────────────────────────────────────────────────────────────────┘
```

---

## 3. System Architecture

```
┌──────────────────────────────────────────────────────────────────────────────┐
│                                 L2 ROLLUP                                    │
│                                                                              │
│  ┌───────────────────────────────┐        ┌────────────────────────────────┐ │
│  │      Sequencer + Workers      │        │   Data Availability (DA)       │ │
│  │  - builds L2 blocks           │  put   │   Backend v1: Google-hosted    │ │
│  │  - posts BlockData            ├────────►   Backend v2+: Celestia/etc    │ │
│  └───────────────┬───────────────┘        └───────────────┬────────────────┘ │
│                  │                                        │                  │
│                  │ read DA                                │ public access    │
│                  ▼                                        ▼                  │
│  ┌────────────────────────────────────────────────────────────────────────┐  │
│  │                              Full Node                                 │  │
│  │  - downloads blocks from DA                                            │  │
│  │  - forms batches every N blocks                                        │  │
│  │  - calls TEE host                                                      │  │
│  │                                                                        │  │
│  │     ┌──────────────────────────────────────────────────────────────┐   │  │
│  │     │           TEE Enclave (Google Confidential VM / AMD SEV)    │   │  │
│  │     │  - re-executes N blocks deterministically                    │   │  │
│  │     │  - computes prev_state_root -> post_state_root               │   │  │
│  │     │  - computes batch_hash (DA commitment)                       │   │  │
│  │     │  - computes withdraw_root from L2 withdrawal messages        │   │  │
│  │     │  - emits Quote where report_data = H(batch_public_data)      │   │  │
│  │     └──────────────────────────────────────────────────────────────┘   │  │
│  │                                                                        │  │
│  └───────────────────────────────┬────────────────────────────────────────┘  │
└──────────────────────────────────┼───────────────────────────────────────────┘
                                   │ submit (HTTP)
                                   ▼
┌──────────────────────────────────────────────────────────────────────────────┐
│                    Security Council (Multisig Verifiers)                     │
│  - verifies attestation signature chain + collateral                         │
│  - verifies VM image measurement matches approved list                       │
│  - verifies report_data == H(batch_public_data)                              │
│  - optionally checks DA availability for the referenced batch range          │
│  - executes finalizeBatch() on L1 (threshold signature required)             │
│  - only authorized finalizer on L1 contract                                  │
└──────────────────────────────────┬───────────────────────────────────────────┘
                                   ▼
┌──────────────────────────────────────────────────────────────────────────────┐
│                                L1 Contract                                   │
│  - stores finalizedStateRoots[batch]                                         │
│  - stores withdrawRoots[batch]                                               │
│  - stores committedBatches[batch]                                            │
│  - verifies chain continuity (prev root/hash matches last)                   │
└──────────────────────────────────────────────────────────────────────────────┘
```

### Component Responsibilities

| Component | Responsibility |
|-----------|---------------|
| **Sequencer** | Orders transactions, builds L2 blocks, posts to DA |
| **Data Availability** | Stores block data for public retrieval |
| **Full Node** | Downloads blocks, forms batches, invokes TEE |
| **TEE Enclave** | Re-executes blocks, produces attestation |
| **Security Council** | Verifies attestations + submits finalization to L1 (multisig) |
| **L1 Contract** | Stores canonical state roots and withdrawal roots |

---

## 4. L1 Contract Storage

The main rollup contract on L1 stores the following data:

### 4.1 Core State Mappings

```solidity
/// @notice Batch index → batch hash
/// @dev Sparse: only stores last batch hash per commit transaction
mapping(uint256 => bytes32) public committedBatches;

/// @notice Batch index → L2 state root
/// @dev Sparse: only stores last state root per finalized bundle
mapping(uint256 => bytes32) public finalizedStateRoots;

/// @notice Batch index → withdrawal merkle root
/// @dev Sparse: only stores last withdraw root per finalized bundle
mapping(uint256 => bytes32) public withdrawRoots;
```

### 4.2 Progress Tracking

```solidity
struct RollupMiscData {
    uint64 lastCommittedBatchIndex;    // Latest batch committed to L1
    uint64 lastFinalizedBatchIndex;    // Latest batch with verified attestation
    uint32 lastFinalizeTimestamp;      // When last finalization occurred
    uint8 flags;                       // Bit flags for system state
    uint88 reserved;                   // Future use
}

RollupMiscData public miscData;
```

### 4.3 Access Control

```solidity
/// @notice Authorized sequencers who can commit batches
mapping(address => bool) public isSequencer;

/// @notice Authorized finalizers (Security Council members)
mapping(address => bool) public isFinalizer;
```

### 4.4 TEE-Specific Storage

```solidity
/// @notice Approved VM image measurements (AMD SEV-SNP measurement values)
/// @dev ⚠️ POLICY ANCHOR: Used for governance/auditing. Enforcement is OFF-CHAIN by council.
/// @dev This is the hash of the Confidential VM image that is authorized to produce attestations.
/// @dev The L1 contract does NOT parse TEE attestations - council verifies measurements off-chain.
mapping(bytes32 => bool) public approvedMeasurements;

/// @notice Security Council member addresses for threshold signature verification
mapping(address => bool) public councilMembers;

/// @notice Ordered list of council members for bitmap-based signature aggregation
address[] public councilMemberList;

/// @notice Minimum council signatures required for finalization
uint256 public signatureThreshold;

/// @notice Current council epoch (incremented on membership changes)
/// @dev Used in councilDigest to prevent signature replay across council rotations
uint64 public councilEpoch;

/// @notice Minimum required TCB (Trusted Computing Base) version
/// @dev ⚠️ POLICY ANCHOR: Used for governance/auditing. Enforcement is OFF-CHAIN by council.
/// @dev Council members reject attestations with TCB version < minTcbVersion.
uint64 public minTcbVersion;

/// @notice Unique identifier for this rollup instance
/// @dev Used in councilDigest for domain separation across rollup deployments
bytes32 public immutable rollupId;
```

**Note on Policy Anchors**: `approvedMeasurements` and `minTcbVersion` are stored on-chain for **governance and auditing** purposes. They allow external observers to verify what TEE configurations the council should be accepting. However, the actual enforcement happens **off-chain** in council verification - the L1 contract does not parse or verify raw TEE attestations.

### 4.5 Immutable Configuration

```solidity
uint64 public immutable layer2ChainId;
address public immutable messageQueue;      // L1MessageQueue address
address public immutable systemConfig;      // System configuration
```

---

## 5. L1 Message Queue Storage

The message queue handles L1 → L2 message ordering and commitment.

### 5.1 Message Storage

```solidity
/// @notice Rolling hash of all L1 → L2 messages
/// @dev Encoding: [32 bits timestamp | 224 bits rolling hash]
mapping(uint256 => bytes32) private messageRollingHashes;

/// @notice First message index in this contract (V1 messages before this)
uint256 public firstCrossDomainMessageIndex;

/// @notice Next message index to be assigned
uint256 public nextCrossDomainMessageIndex;

/// @notice First message not yet finalized on L2
uint256 public nextUnfinalizedQueueIndex;
```

### 5.2 Rolling Hash Computation

For each new L1 → L2 message:

```
hash[0] = H(msg0)
hash[n] = H(hash[n-1], H(msg_n))
```

This creates a commitment to ALL messages in order, verified by the TEE.

---

## 6. TEE Attestation Flow

The TEE produces the **exact same outputs** as a ZK proof would — the difference is only in how those outputs are proven correct (hardware attestation vs mathematical proof).

### 6.0 TEE Output = ZK Proof Output

The TEE must generate public data analogous to ZK circuit public inputs. This data is **consensus-critical** and must be precisely specified.

| Field | Type | Role | Description |
|-------|------|------|-------------|
| `version` | uint32 | META | Protocol version (= 1) |
| `layer2ChainId` | uint64 | INPUT | Ensures attestation is for correct L2 chain |
| `rollupId` | bytes32 | INPUT | Domain separation across rollup instances |
| `batchIndex` | uint64 | INPUT | The batch being finalized |
| `daStartHeight` | uint64 | INPUT | DA block range start (inclusive) |
| `daEndHeight` | uint64 | INPUT | DA block range end (inclusive) |
| `daCommitment` | bytes32 | INPUT | Merkle root of DA block hashes |
| `lastProcessedQueueIndex` | uint256 | INPUT | Last L1→L2 message index processed |
| `messageQueueHash` | bytes32 | INPUT | Rolling hash at lastProcessedQueueIndex |
| `prevStateRoot` | bytes32 | INPUT | Starting state (from L1 storage) |
| `prevBatchHash` | bytes32 | INPUT | Previous batch hash (chain continuity) |
| `postStateRoot` | bytes32 | **OUTPUT** | New L2 state root after execution |
| `batchHash` | bytes32 | **OUTPUT** | Hash of this batch |
| `withdrawRoot` | bytes32 | **OUTPUT** | Merkle root of L2→L1 withdrawals |

```
┌─────────────────────────────────────────────────────────────────┐
│                 TEE vs ZK: Same Interface, Different Trust      │
├─────────────────────────────────────────────────────────────────┤
│                                                                 │
│  ZK ROLLUP                           TEE ROLLUP                 │
│  ─────────                           ──────────                 │
│                                                                 │
│  Execute off-chain                   Execute in Confidential VM │
│        ↓                                   ↓                    │
│  Generate ZK proof                   Generate SEV-SNP Attestation│
│  (mathematical proof)                (hardware attestation)     │
│        ↓                                   ↓                    │
│  Submit to L1:                       Submit to Security Council:│
│  • publicInputs                      • batchPublicData          │
│  • zkProof                           • teeQuote                 │
│        ↓                                   ↓                    │
│  L1 verifies:                        Council verifies off-chain:│
│  verifyProof(proof, inputs)          verifyQuote(quote, data)   │
│        ↓                                   ↓                    │
│  Store: stateRoot, withdrawRoot      L1 verifies threshold sigs │
│                                      Store: stateRoot, withdrawRoot
│                                                                 │
│  ⚠️ SAME PUBLIC OUTPUTS AND L1 INTERFACE                        │
│  ⚠️ DIFFERENT TRUST ASSUMPTIONS:                                │
│     ZK: Mathematical (trustless)                                │
│     TEE: Hardware attestation + threshold signers               │
│                                                                 │
└─────────────────────────────────────────────────────────────────┘
```

### 6.1 TEE Execution

The TEE enclave receives batch data and produces an attestation:

```
┌─────────────────────────────────────────────────────────────────┐
│                     TEE Enclave Execution                       │
├─────────────────────────────────────────────────────────────────┤
│                                                                 │
│  INPUTS (from Full Node)                                        │
│  ────────────────────────                                       │
│  • Block data from DA (transactions, headers)                   │
│  • Previous state root (from last finalized batch)              │
│  • L1 → L2 message queue data                                   │
│                                                                 │
│  DETERMINISTIC EXECUTION                                        │
│  ────────────────────────                                       │
│  1. Validate block headers chain correctly                      │
│  2. Execute all transactions in order                           │
│  3. Process all L1 → L2 messages                                │
│  4. Collect L2 → L1 withdrawal messages                         │
│  5. Compute final state root                                    │
│  6. Build withdrawal merkle tree                                │
│                                                                 │
│  OUTPUTS (in attestation)                                       │
│  ────────────────────────                                       │
│  • prev_state_root     (input verification)                     │
│  • post_state_root     (execution result)                       │
│  • withdraw_root       (L2 → L1 messages)                       │
│  • batch_hash          (DA commitment)                          │
│  • message_queue_hash  (L1 → L2 messages processed)             │
│  • batch_index         (which batch this is)                    │
│  • layer2_chain_id     (chain identifier)                       │
│                                                                 │
└─────────────────────────────────────────────────────────────────┘
```

### 6.2 Quote Generation

The TEE produces a quote (attestation) containing all public data. This is the **critical output** that proves correct execution:

```solidity
/// @notice Consensus-critical public data committed into TEE report_data and signed by the council.
/// @dev All fields are fixed-size to make ABI encoding deterministic across implementations.
/// @dev Uses abi.encode (NOT abi.encodePacked) for canonical encoding.
struct BatchPublicDataV1 {
    uint32  version;                    // = 1 (protocol version)
    uint64  layer2ChainId;              // Cross-chain replay protection
    bytes32 rollupId;                   // Domain separation across rollup instances
    uint64  batchIndex;                 // The batch being finalized
    
    // DA commitment for the batch
    uint64  daStartHeight;              // DA block range start (inclusive)
    uint64  daEndHeight;                // DA block range end (inclusive)
    bytes32 daCommitment;               // Merkle root of DA block hashes in [start,end]
    
    // L1->L2 message queue progress
    uint256 lastProcessedQueueIndex;    // Last message index processed (inclusive)
    bytes32 messageQueueHash;           // Rolling hash at lastProcessedQueueIndex
    
    // State transition and continuity
    bytes32 prevStateRoot;              // INPUT: Starting state (must match L1 storage)
    bytes32 prevBatchHash;              // INPUT: Parent batch hash (chain continuity)
    bytes32 postStateRoot;              // OUTPUT: New state root after execution
    bytes32 batchHash;                  // OUTPUT: Hash of this batch
    
    // Withdrawals produced during execution
    bytes32 withdrawRoot;               // OUTPUT: Merkle root of L2→L1 withdrawal messages
}
```

### 6.2.1 Canonical Hashing and `report_data` Binding

The TEE commits to `BatchPublicDataV1` by including its hash in the attestation's `report_data` field:

```solidity
/// Canonical commitment used by TEE and all verifiers:
/// bpdHash = poseidon2(abi.encode(BatchPublicDataV1))
bytes32 bpdHash = poseidon2(abi.encode(batchPublicDataV1));

/// TEE binding rule (AMD SEV-SNP report_data is 64 bytes):
/// quote.report_data[0:32]  == bpdHash
/// quote.report_data[32:64] == 0x00..00  (reserved for future use in v1)
```

**Critical**: Use `abi.encode`, NOT `abi.encodePacked`. This ensures deterministic encoding with proper padding.

#### Why Each Field Matters

```
┌─────────────────────────────────────────────────────────────────┐
│                    FIELD PURPOSE BREAKDOWN                      │
├─────────────────────────────────────────────────────────────────┤
│                                                                 │
│  version                                                        │
│  └── Protocol version for forward compatibility                 │
│      Allows upgrading BatchPublicData format in the future      │
│                                                                 │
│  layer2ChainId + rollupId                                       │
│  └── Domain separation: prevents cross-chain/cross-rollup replay│
│      "This attestation is specifically for chain 534352,        │
│       rollup instance 0xabc..."                                 │
│                                                                 │
│  batchIndex                                                     │
│  └── Explicit batch number being finalized                      │
│      L1 verifies: batchIndex == lastFinalizedBatchIndex + 1     │
│      "I am finalizing batch #N"                                 │
│                                                                 │
│  daStartHeight + daEndHeight + daCommitment                     │
│  └── Precise DA commitment for the batch                        │
│      "I executed DA blocks [start, end] with commitment 0x..."  │
│      Council verifies: can reconstruct daCommitment from DA     │
│                                                                 │
│  lastProcessedQueueIndex + messageQueueHash                     │
│  └── Proves ALL L1→L2 deposits were processed (with index!)    │
│      L1 verifies:                                               │
│        messageQueueHash == L1MessageQueue.getHash(lastIdx)      │
│      "I executed all messages up to index N with hash 0xabc"    │
│                                                                 │
│  prevStateRoot                                                  │
│  └── Starting point for execution (INPUT)                       │
│      L1 verifies: prevStateRoot == finalizedStateRoots[prevIdx] │
│      "I started execution from this known-good state"           │
│                                                                 │
│  prevBatchHash                                                  │
│  └── Chain continuity (INPUT)                                   │
│      L1 verifies: prevBatchHash == committedBatches[prevIdx]    │
│      "My parent batch is this one"                              │
│                                                                 │
│  postStateRoot ⭐ KEY OUTPUT                                    │
│  └── Result of executing all transactions                       │
│      "After executing everything, the new state root is 0xdef"  │
│      Gets stored in: finalizedStateRoots[batchIndex]            │
│                                                                 │
│  batchHash ⭐ KEY OUTPUT                                        │
│  └── Identifies the batch (commits to DA data)                  │
│      L1 verifies: batchHash == committedBatches[batchIndex]     │
│      "I executed the batch that was committed with this hash"   │
│                                                                 │
│  withdrawRoot ⭐ KEY OUTPUT                                     │
│  └── Merkle root of all L2→L1 withdrawal messages               │
│      "Users can prove withdrawals against this root"            │
│      Gets stored in: withdrawRoots[batchIndex]                  │
│                                                                 │
└─────────────────────────────────────────────────────────────────┘
```

#### The Attestation Statement

When the TEE produces this output, it's making the following cryptographic statement:

> "I am a genuine Google Confidential VM running on AMD SEV-SNP hardware.
> My VM image matches measurement `X`.
> I executed DA blocks [`daStartHeight`, `daEndHeight`] with commitment `daCommitment`.
> I processed L1→L2 messages up to index `lastProcessedQueueIndex` with hash `messageQueueHash`.
> Starting from `prevStateRoot` (batch `batchIndex - 1`), after processing all transactions,
> the resulting state is `postStateRoot`, batch hash is `batchHash`, and withdrawal root is `withdrawRoot`."

This is **semantically similar** to what a ZK proof asserts, but with fundamentally different trust assumptions (hardware + threshold signers vs mathematical proof).

### 6.2.2 Council Signature Digest (Domain Separation)

Council members do NOT sign the raw `bpdHash`. They sign a **domain-separated digest** to prevent replay attacks across chains, contracts, and epochs:

```solidity
/// @notice Domain separator for council attestation signatures
bytes32 constant COUNCIL_DOMAIN_SEP = poseidon2("TEE_ROLLUP_COUNCIL_ATTESTATION_V1");

/// @notice Compute the digest that council members sign
/// @dev This prevents replay attacks across:
///      - Different L2 chains (layer2ChainId)
///      - Different rollup instances (rollupId)  
///      - Different batches (batchIndex)
///      - Different council epochs (councilEpoch)
///      - Different L1 contracts (rollupChain address)
function councilDigest(
    bytes32 bpdHash,
    uint64  layer2ChainId,
    bytes32 rollupId,
    uint64  batchIndex,
    uint64  councilEpoch,
    address rollupChain
) internal pure returns (bytes32) {
    return poseidon2(abi.encode(
        COUNCIL_DOMAIN_SEP,
        layer2ChainId,
        rollupId,
        batchIndex,
        councilEpoch,
        rollupChain,
        bpdHash
    ));
}

/// Council members sign: sign(councilDigest(...))
/// L1 contract verifies threshold signatures over this digest
```

**Why `councilEpoch`?** Allows rotating council membership without risking signature replay from old councils.

### 6.3 Attestation Structure (Google Confidential Computing)

Google Confidential VMs use **AMD SEV-SNP** (Secure Encrypted Virtualization - Secure Nested Paging) for hardware-based attestation.

```
┌────────────────────────────────────────────────────────────┐
│              Google Confidential VM Attestation            │
├────────────────────────────────────────────────────────────┤
│                                                            │
│  AMD SEV-SNP Attestation Report                            │
│  ├── version: uint32                                       │
│  ├── guest_svn: uint32      (guest security version)       │
│  ├── policy: uint64         (VM policy flags)              │
│  ├── measurement: bytes32   (VM image measurement)         │
│  ├── host_data: bytes32     (host-provided data)           │
│  ├── report_data: bytes64   ← YOUR CUSTOM DATA             │
│  │   └── [0:32] = H(batch_public_data)                     │
│  ├── chip_id: bytes64       (unique chip identifier)       │
│  └── signature: bytes512    (AMD signed)                   │
│                                                            │
│  Google-Specific Wrapper                                   │
│  ├── gce_metadata          (instance info, project, zone)  │
│  ├── timestamp             (when attestation was generated)│
│  └── certificate_chain     (AMD → Google signing chain)    │
│                                                            │
└────────────────────────────────────────────────────────────┘
```

#### Key Fields for Verification

| Field | Purpose |
|-------|---------|
| `measurement` | Hash of VM image — ensures correct code is running (like MRENCLAVE) |
| `report_data` | Custom 64 bytes — contains H(batchPublicData) |
| `policy` | VM policy flags — ensures required security features enabled |
| `signature` | AMD's signature — proves report came from genuine AMD CPU |
| `certificate_chain` | Links AMD root → Google → attestation |

### 6.4 Security Council Verification

The Security Council verifies attestations **off-chain** and submits threshold signatures to L1.

#### 6.4.1 Off-Chain Verification (Council Nodes)

Each council node independently verifies:

1. **Signature Chain**: Valid AMD → Google certificate chain
2. **Measurement**: VM image hash matches approved measurement (from governance)
3. **Report Data**: `quote.report_data[0:32] == poseidon2(abi.encode(batchPublicData))`
4. **Policy Flags**: Required security features are enabled (SEV-SNP, etc.)
5. **TCB Version**: Trusted Computing Base is not revoked/outdated
6. **DA Verification**: Can reconstruct `daCommitment` from DA blocks `[daStartHeight, daEndHeight]`
7. **Continuity**: `prevBatchHash` and `prevStateRoot` match expected values

If all checks pass, the council member signs the `councilDigest(...)`.

#### 6.4.2 On-Chain Verification (L1 Contract)

The L1 contract does **NOT** parse or verify the raw TEE attestation. It only:

1. Decodes `BatchPublicDataV1` from the submitted bytes
2. Recomputes `bpdHash = poseidon2(abi.encode(batchPublicData))`
3. Computes `councilDigest(bpdHash, ...)`
4. Verifies threshold signatures over that digest
5. Checks state continuity against stored values
6. Stores results if all checks pass

```
┌─────────────────────────────────────────────────────────────────┐
│           ON-CHAIN vs OFF-CHAIN VERIFICATION                    │
├─────────────────────────────────────────────────────────────────┤
│                                                                 │
│  OFF-CHAIN (Security Council)          ON-CHAIN (L1 Contract)   │
│  ─────────────────────────────         ────────────────────────  │
│  ✓ Verify AMD/Google sig chain         ✗ Does NOT verify quote  │
│  ✓ Check VM measurement                ✗ Does NOT parse quote   │
│  ✓ Check TCB version                   ✗ Does NOT check TCB     │
│  ✓ Verify report_data binding          ✓ Verifies council sigs  │
│  ✓ Reconstruct DA commitment           ✓ Checks state continuity│
│  ✓ Sign councilDigest                  ✓ Stores finalized state │
│                                                                 │
│  ⚠️ approvedMeasurements and minTcbVersion stored on-chain are  │
│     POLICY ANCHORS for governance/auditing, not enforcement.    │
│     Enforcement happens off-chain in council verification.      │
│                                                                 │
└─────────────────────────────────────────────────────────────────┘
```

---

## 7. Batch Lifecycle

### 7.1 Commit Phase

```
Sequencer                              L1 Contract
────────                               ───────────

1. Build N L2 blocks
2. Post block data to DA
3. Compute batch hash
        │
        │  commitBatch(batchHash, parentBatchHash)
        ▼
                                       4. Verify parentBatchHash matches
                                          committedBatches[lastIndex]
                                       5. Store committedBatches[newIndex]
                                       6. Update lastCommittedBatchIndex
                                       7. Emit CommitBatch event
```

### 7.2 Attestation Phase

```
Full Node                 TEE Enclave              Security Council
─────────                 ───────────              ────────────────

1. Download blocks
   from DA
        │
        │  execute(blocks, prevState)
        ▼
                          2. Re-execute all txs
                          3. Compute state roots
                          4. Build withdraw tree
                          5. Generate attestation
        │
        │  attestation + batchPublicData
        ▼
                                                   6. Verify attestation chain
                                                   7. Check VM measurement
                                                   8. Validate report_data
                                                   9. Collect threshold sigs
```

### 7.3 Finalization Phase

```
Security Council                                   L1 Contract
────────────────                                   ───────────

1. Receive attestation from TEE
2. Verify attestation validity
3. Collect threshold signatures
        │
        │  finalizeBatch(batchHeader, stateRoot,
        │                withdrawRoot, councilSignatures)
        ▼
                                                   4. Verify council signatures
                                                   5. Check signature threshold
                                                   6. Verify chain continuity
                                                   7. Store finalizedStateRoots
                                                   8. Store withdrawRoots
                                                   9. Update message queue
                                                   10. Emit FinalizeBatch
```

---

## 8. L1 → L2 Deposits

### 8.1 Overview

```
┌─────────────────────────────────────────────────────────────────────────────┐
│                                   L1                                        │
├─────────────────────────────────────────────────────────────────────────────┤
│                                                                             │
│  1. USER calls depositNIGHT(amount, gasLimit)                               │
│     │                                                                       │
│     ▼                                                                       │
│  2. L1NIGHTGateway                                                          │
│     ├── Locks NIGHT in gateway/messenger                                    │
│     ├── Encodes message: finalizeDepositNIGHT(from, to, amount)             │
│     └── Calls L1Messenger.sendMessage()                                     │
│     │                                                                       │
│     ▼                                                                       │
│  3. L1Messenger                                                             │
│     ├── Assigns nonce from nextCrossDomainMessageIndex                      │
│     ├── Computes message hash                                               │
│     ├── Collects fee for L2 gas                                             │
│     └── Calls L1MessageQueue.appendCrossDomainMessage()                     │
│     │                                                                       │
│     ▼                                                                       │
│  4. L1MessageQueue                                                          │
│     ├── Computes transaction hash                                           │
│     ├── Updates rolling hash: hash[n] = H(hash[n-1], txHash)                │
│     ├── Stores timestamp with rolling hash                                  │
│     ├── Increments nextCrossDomainMessageIndex                              │
│     └── Emits QueueTransaction event                                        │
│                                                                             │
└─────────────────────────────────────────────────────────────────────────────┘
                                    │
                                    │ Sequencer monitors QueueTransaction events
                                    │ Includes L1 messages in L2 blocks
                                    │
┌─────────────────────────────────────────────────────────────────────────────┐
│                                   L2                                        │
├─────────────────────────────────────────────────────────────────────────────┤
│                                                                             │
│  5. Sequencer includes L1 message as special transaction (type 0x7E)        │
│     │                                                                       │
│     ▼                                                                       │
│  6. L2Messenger.relayMessage() executes automatically                       │
│     ├── Verifies sender is aliased L1Messenger                              │
│     ├── Sets xDomainMessageSender = L1NIGHTGateway                          │
│     └── Forwards call to L2NIGHTGateway                                     │
│     │                                                                       │
│     ▼                                                                       │
│  7. L2NIGHTGateway.finalizeDepositNIGHT()                                   │
│     ├── Mints/transfers NIGHT to recipient                                  │
│     └── Emits FinalizeDepositNIGHT event                                    │
│                                                                             │
│  ✓ USER NOW HAS NIGHT ON L2                                                 │
│                                                                             │
└─────────────────────────────────────────────────────────────────────────────┘
```

### 8.2 Message Hash Computation

```solidity
// L1 message is encoded as special transaction type 0x7E
// TransactionPayload = rlp([queueIndex, gasLimit, to, value, data, sender])
bytes32 txHash = poseidon2(0x7E || rlp([
    queueIndex,
    gasLimit,
    target,
    value,
    data,
    sender
]));
```

### 8.3 Rolling Hash Chain

```
Message 0: rollingHash[0] = H(txHash_0)
Message 1: rollingHash[1] = H(rollingHash[0], txHash_1)
Message 2: rollingHash[2] = H(rollingHash[1], txHash_2)
...
Message N: rollingHash[N] = H(rollingHash[N-1], txHash_N)
                                    │
                                    └── This hash is included in TEE attestation
                                        to prove ALL messages were processed
```

### 8.4 TEE Verification of L1 Messages

The TEE enclave verifies L1 → L2 message processing and the L1 contract enforces correctness:

```
TEE Execution:
1. Input: L1 message queue data (messages, rolling hashes)
2. Execute: Process all L1 messages included in L2 blocks
3. Output: Include BOTH lastProcessedQueueIndex AND messageQueueHash in BatchPublicDataV1
```

#### 8.4.1 L1 Contract Message Queue Verification

The L1 contract performs **index-based verification**:

```solidity
/// @notice Verify message queue progress and advance the finalization pointer
/// @param lastProcessedQueueIndex The last message index processed by TEE (inclusive)
/// @param messageQueueHash The rolling hash at lastProcessedQueueIndex
function verifyAndAdvanceMessageQueue(
    uint256 lastProcessedQueueIndex,
    bytes32 messageQueueHash
) internal {
    // 1. Verify the rolling hash matches L1MessageQueue at this index
    require(
        messageQueueHash == 
            IL1MessageQueue(messageQueue).getMessageRollingHash(lastProcessedQueueIndex),
        "BAD_MESSAGE_QUEUE_HASH"
    );
    
    // 2. Verify monotonic progress (no skipping or going backwards)
    uint256 currentUnfinalized = IL1MessageQueue(messageQueue).nextUnfinalizedQueueIndex();
    require(
        lastProcessedQueueIndex >= currentUnfinalized - 1,
        "REGRESSED_MESSAGE_QUEUE"
    );
    
    // 3. Advance the unfinalized pointer
    // nextUnfinalizedQueueIndex = lastProcessedQueueIndex + 1
    IL1MessageQueue(messageQueue).finalizePoppedCrossDomainMessage(
        lastProcessedQueueIndex + 1
    );
}
```

**Why is the index required?** The `messageQueueHash` alone is not sufficient because:
- The L1 contract cannot derive which index produced a given rolling hash
- Without the index, the contract cannot verify monotonic progress
- The TEE must explicitly commit to how many messages it processed

---

## 9. L2 → L1 Withdrawals

### 9.1 Overview

```
┌─────────────────────────────────────────────────────────────────────────────┐
│                                   L2                                        │
├─────────────────────────────────────────────────────────────────────────────┤
│                                                                             │
│  1. USER calls withdrawNIGHT(amount, gasLimit)                              │
│     │                                                                       │
│     ▼                                                                       │
│  2. L2NIGHTGateway                                                          │
│     ├── Burns/locks NIGHT on L2                                             │
│     ├── Encodes message: finalizeWithdrawNIGHT(from, to, amount)            │
│     └── Calls L2Messenger.sendMessage()                                     │
│     │                                                                       │
│     ▼                                                                       │
│  3. L2Messenger                                                             │
│     ├── Assigns nonce from L2MessageQueue.nextMessageIndex                  │
│     ├── Computes message hash (xDomainCalldataHash)                         │
│     └── Calls L2MessageQueue.appendMessage(hash)                            │
│     │                                                                       │
│     ▼                                                                       │
│  4. L2MessageQueue (Append-Only Merkle Tree)                                │
│     ├── Adds message hash as new leaf                                       │
│     ├── Recomputes merkle tree                                              │
│     ├── Updates messageRoot (= withdrawRoot)                                │
│     └── Emits AppendMessage event                                           │
│                                                                             │
└─────────────────────────────────────────────────────────────────────────────┘
                                    │
                                    │ TEE includes withdrawRoot in attestation
                                    │ Batch finalized on L1 with withdrawRoot
                                    │
                                    │ ⏳ WAIT FOR FINALIZATION
                                    │
┌─────────────────────────────────────────────────────────────────────────────┐
│                                   L1                                        │
├─────────────────────────────────────────────────────────────────────────────┤
│                                                                             │
│  5. USER (or relayer) calls relayMessageWithProof()                         │
│     │                                                                       │
│     │  Parameters:                                                          │
│     │  - from, to, value, nonce, message (withdrawal details)               │
│     │  - batchIndex (which batch contains the withdrawal)                   │
│     │  - merkleProof (proof against withdrawRoot)                           │
│     │                                                                       │
│     ▼                                                                       │
│  6. L1Messenger                                                             │
│     ├── Computes xDomainCalldataHash from parameters                        │
│     ├── Checks: !isL2MessageExecuted[hash] (not already claimed)            │
│     ├── Checks: isBatchFinalized(batchIndex)                                │
│     ├── Gets: withdrawRoot = withdrawRoots[batchIndex]                      │
│     ├── Verifies: merkleProof against withdrawRoot                          │
│     └── Executes: calls L1NIGHTGateway with message                         │
│     │                                                                       │
│     ▼                                                                       │
│  7. L1NIGHTGateway.finalizeWithdrawNIGHT()                                  │
│     ├── Transfers NIGHT to recipient                                        │
│     └── Emits FinalizeWithdrawNIGHT event                                   │
│                                                                             │
│  ✓ USER NOW HAS NIGHT ON L1                                                 │
│                                                                             │
└─────────────────────────────────────────────────────────────────────────────┘
```

### 9.2 Withdraw Merkle Tree

The L2MessageQueue maintains an append-only merkle tree:

```
                    withdrawRoot (messageRoot)
                           │
              ┌────────────┴────────────┐
              │                         │
           hash01                    hash23
          /    \                    /    \
        h0      h1                h2      h3
        │       │                 │       │
       msg0    msg1              msg2    msg3
        │       │                 │       │
   withdraw  withdraw         withdraw  withdraw
      #0       #1                #2       #3
```

### 9.3 Merkle Proof Structure

```solidity
struct L2MessageProof {
    uint256 batchIndex;     // Which finalized batch
    bytes merkleProof;      // Sibling hashes from leaf to root
}

// Verification:
// 1. Compute leaf = poseidon2(xDomainCalldata)
// 2. Walk up tree using merkleProof siblings
// 3. Compare computed root with withdrawRoots[batchIndex]
```

### 9.4 Efficient Tree Updates

```solidity
// Storage for O(log n) updates:
uint256 public nextMessageIndex;      // Current leaf count
bytes32[40] public branches;          // Cached intermediate hashes
bytes32[40] private zeroHashes;       // Pre-computed empty subtree hashes

// Adding a message only requires ~40 hash operations, not rebuilding entire tree
```

---

## 10. Data Availability

### 10.1 DA Backends

| Backend | Description | Trust Model |
|---------|-------------|-------------|
| **Google-hosted** | Centralized storage | Trust operator |
| **Celestia** | Dedicated DA layer | Celestia consensus |
| **Midnight DA** | Native DA solution | Midnight consensus |

### 10.2 DA Commitment Specification

The `daCommitment` in `BatchPublicDataV1` is a **Merkle root** of DA block identifiers. This is **consensus-critical** and must be computed identically by TEE and council verifiers.

#### 10.2.1 DA Commitment Algorithm

```solidity
/// @notice Compute the DA commitment for a range of DA blocks
/// @param daStartHeight First DA block height (inclusive)
/// @param daEndHeight Last DA block height (inclusive)
/// @param blockIds Array of DA block identifiers/hashes
/// @return daCommitment Merkle root of the block IDs
function computeDACommitment(
    uint64 daStartHeight,
    uint64 daEndHeight,
    bytes32[] memory blockIds
) internal pure returns (bytes32) {
    require(blockIds.length == daEndHeight - daStartHeight + 1, "WRONG_BLOCK_COUNT");
    
    // 1. Compute leaves with domain separation
    bytes32[] memory leaves = new bytes32[](blockIds.length);
    for (uint i = 0; i < blockIds.length; i++) {
        // Leaf prefix 0x00 for domain separation
        leaves[i] = poseidon2(abi.encodePacked(bytes1(0x00), blockIds[i]));
    }
    
    // 2. Pad to power of 2 by duplicating last leaf
    uint256 paddedLength = nextPowerOfTwo(leaves.length);
    bytes32[] memory paddedLeaves = new bytes32[](paddedLength);
    for (uint i = 0; i < paddedLength; i++) {
        paddedLeaves[i] = i < leaves.length ? leaves[i] : leaves[leaves.length - 1];
    }
    
    // 3. Build Merkle tree
    return merkleRoot(paddedLeaves);
}

/// @notice Compute Merkle root from leaves
function merkleRoot(bytes32[] memory leaves) internal pure returns (bytes32) {
    if (leaves.length == 1) return leaves[0];
    
    bytes32[] memory nextLevel = new bytes32[](leaves.length / 2);
    for (uint i = 0; i < nextLevel.length; i++) {
        // Internal node prefix 0x01 for domain separation
        nextLevel[i] = poseidon2(abi.encodePacked(
            bytes1(0x01),
            leaves[2*i],
            leaves[2*i + 1]
        ));
    }
    return merkleRoot(nextLevel);
}
```

#### 10.2.2 DA Commitment Encoding Rules

| Rule | Specification |
|------|---------------|
| **Hash function** | poseidon2 |
| **Leaf prefix** | `0x00` (prevents second-preimage attacks) |
| **Node prefix** | `0x01` (domain separation) |
| **Padding** | Duplicate last leaf to reach power-of-2 |
| **Block ID** | Backend-specific (e.g., Celestia blob commitment, block hash) |
| **Empty range** | `daCommitment = bytes32(0)` when no DA blocks |

#### 10.2.3 Batch Hash Computation

The `batchHash` in `BatchPublicDataV1` is computed as:

```solidity
bytes32 batchHash = poseidon2(abi.encode(
    uint8(1),              // batchVersion
    batchIndex,
    parentBatchHash,
    daCommitment,
    daStartHeight,
    daEndHeight
));
```

### 10.3 TEE DA Verification

The TEE enclave must:

1. Receive block data from DA for heights `[daStartHeight, daEndHeight]`
2. Compute `daCommitment` using the algorithm above
3. Execute all blocks in that range
4. Include `daCommitment`, `daStartHeight`, `daEndHeight` in `BatchPublicDataV1`

### 10.4 Council DA Verification

Council members independently verify DA availability:

1. Fetch DA blocks for heights `[daStartHeight, daEndHeight]`
2. Recompute `daCommitment` using the same algorithm
3. Verify it matches the value in `BatchPublicDataV1`
4. **Reject attestation if DA is unavailable or commitment mismatches**

---

## 11. Security Model

### 11.1 Trust Assumptions

This specification assumes **AMD SEV-SNP on Google Confidential VMs**. The trust model is:

```
┌─────────────────────────────────────────────────────────────────┐
│              Security Trust Stack (AMD SEV-SNP Specific)        │
├─────────────────────────────────────────────────────────────────┤
│                                                                 │
│  Layer 1: AMD Hardware (SEV-SNP)                                │
│  ───────────────────────────────                                │
│  Trust: AMD CPU correctly enforces memory encryption and        │
│         generates authentic attestation reports                 │
│  Risk: Hardware vulnerabilities, side-channel attacks,          │
│         AMD key compromise                                      │
│  Mitigation:                                                    │
│    • TCB version checks (reject outdated firmware)              │
│    • AMD security advisories monitoring                         │
│    • Council can update minTcbVersion on CVE disclosure         │
│                                                                 │
│  Layer 2: Google Infrastructure                                 │
│  ───────────────────────────────                                │
│  Trust: Google correctly provisions Confidential VMs and        │
│         maintains certificate chain to AMD root                 │
│  Risk: Google infrastructure compromise                         │
│  Mitigation:                                                    │
│    • Attestation signed by AMD, not Google                      │
│    • Certificate chain verifiable against AMD root CA           │
│                                                                 │
│  Layer 3: VM Image Code                                         │
│  ───────────────────────────────                                │
│  Trust: VM measurement hash matches audited code                │
│  Risk: Bugs in execution code                                   │
│  Mitigation:                                                    │
│    • Multiple audits of VM image                                │
│    • Formal verification where possible                         │
│    • Timelocked measurement upgrades                            │
│    • approvedMeasurements on-chain for transparency             │
│                                                                 │
│  Layer 4: Security Council (Threshold Signers)                  │
│  ───────────────────────────────────────────────                │
│  Trust: At least `signatureThreshold` members are honest        │
│  Risk: Key compromise, collusion below threshold                │
│  Mitigation:                                                    │
│    • Geographically distributed members                         │
│    • Hardware security modules (HSMs) for keys                  │
│    • Economic incentives / slashing                             │
│    • Council epoch rotation                                     │
│                                                                 │
│  Layer 5: L1 Contract                                           │
│  ───────────────────────────────────────────────                │
│  Trust: Midnight L1 consensus                                   │
│  Risk: L1 reorg, contract bugs                                  │
│  Mitigation:                                                    │
│    • Standard L1 security practices                             │
│    • Multiple audits                                            │
│    • Confirmation depth for finality                            │
│                                                                 │
└─────────────────────────────────────────────────────────────────┘
```

#### 11.1.1 What Can Go Wrong

| Failure Mode | Impact | Detection | Recovery |
|--------------|--------|-----------|----------|
| AMD SEV-SNP vulnerability | Fake attestations possible | Security advisory | Update minTcbVersion, pause if critical |
| VM code bug | Incorrect state transitions | Audits, monitoring | Update approvedMeasurements |
| Council key compromise (< threshold) | No impact | - | Rotate compromised member |
| Council collusion (≥ threshold) | Invalid batches finalized | Impossible to detect | Emergency governance |
| L1 reorg | Temporary inconsistency | Standard L1 monitoring | Wait for deeper confirmation |

### 11.2 Attack Vectors & Mitigations

| Attack | Impact | Mitigation |
|--------|--------|------------|
| **TEE compromise** | Fake attestations | Multiple TEE vendors, council verification |
| **Council collusion** | Accept invalid batches | Threshold signatures, slashing, distribution |
| **Sequencer censorship** | Block user transactions | Forced inclusion mechanism |
| **DA unavailability** | Cannot verify state | Multiple DA backends, timeouts |
| **L1 reorg** | State inconsistency | Confirmation depth requirements |

### 11.3 Forced Inclusion (Anti-Censorship)

If the sequencer censors transactions:

```solidity
// After timeout, anyone can submit batches
function commitAndFinalizeBatch(...) external {
    if (!isEnforcedModeEnabled()) {
        // Check if enough time passed without finalization
        if (
            firstUnfinalizedMessageTime + maxDelayMessageQueue < block.timestamp ||
            lastFinalizeTimestamp + maxDelayEnterEnforcedMode < block.timestamp
        ) {
            // Enable enforced mode - anyone can submit
            enableEnforcedMode();
        }
    }
    // Process batch...
}
```

### 11.4 Upgrade Safety

```
VM Image Measurement Upgrade Process:
1. Security Council proposes new VM image measurement
2. Time-lock period for review (e.g., 7 days)
3. Security Council updates approved measurements list
4. Old measurement deprecated after transition period
5. TCB version requirements updated as needed
```

```solidity
// Example upgrade flow
function proposeNewMeasurement(bytes32 newMeasurement) external onlyOwner {
    pendingMeasurement = newMeasurement;
    measurementActivationTime = block.timestamp + TIMELOCK_PERIOD;
    emit MeasurementProposed(newMeasurement, measurementActivationTime);
}

function activateMeasurement() external {
    require(block.timestamp >= measurementActivationTime, "Timelock not expired");
    approvedMeasurements[pendingMeasurement] = true;
    emit MeasurementActivated(pendingMeasurement);
}
```

---

## 12. Existing Codebase Components

This section maps the TEE rollup requirements to existing code in the sovereign-sdk that can be reused or extended.

### 12.1 Components That Can Be Reused

#### Batch Aggregation Structure

**File:** `crates/rollup-interface/src/state_machine/zk/aggregated_proof.rs`

The existing `AggregatedProofPublicData` structure already captures most of what we need for TEE batch proofs:

```rust
pub struct AggregatedProofPublicData<Address, Da: DaSpec, Root> {
    pub initial_slot_number: SlotNumber,      // ✅ Maps to: batch start
    pub final_slot_number: SlotNumber,        // ✅ Maps to: batch end
    pub genesis_state_root: Root,             // ✅ Keep for verification
    pub initial_state_root: Root,             // ✅ Maps to: prevStateRoot
    pub final_state_root: Root,               // ✅ Maps to: postStateRoot
    pub initial_slot_hash: Da::SlotHash,      // ✅ Maps to: prevBatchHash
    pub final_slot_hash: Da::SlotHash,        // ✅ Maps to: batchHash
    pub code_commitment: CodeCommitment,      // ⚠️ Repurpose for: VM measurement
    pub rewarded_addresses: Vec<Address>,     // ❓ May not need for TEE
}
```

**Extension needed:** Add `withdraw_root` and `message_queue_hash` fields.

#### ZK Manager Orchestration Pattern

**File:** `crates/full-node/sov-stf-runner/src/processes/zk_manager/mod.rs`

The `ZkProofManager` already implements the batching logic we need:

```rust
pub struct ZkProofManager<Ps: ProverService> {
    prover_service: Ps,
    proofs_to_create: UnAggregatedProofList<Ps>,
    aggregated_proof_block_jump: NonZero<usize>,  // ✅ Batch size config
    proof_sender: Box<dyn ProofSender>,
    // ...
}

impl ZkProofManager {
    async fn process_stf_info(&mut self, stf_info: StateTransitionInfo) {
        // Every N blocks, create aggregated proof
        if num_proofs >= self.aggregated_proof_block_jump.get() {
            let agg_proof = self.create_aggregate_proof(...).await?;
            self.proof_sender.publish_proof_blob_with_metadata(agg_proof).await?;
        }
    }
}
```

**Adaptation needed:** Replace ZK proof generation with TEE attestation generation.

#### State Transition Witness

**File:** `crates/rollup-interface/src/state_machine/zk/mod.rs`

Contains all data the TEE needs to re-execute blocks:

```rust
pub struct StateTransitionWitness<StateRoot, Witness, Da: DaSpec> {
    pub initial_state_root: StateRoot,    // ✅ Input to TEE
    pub final_state_root: StateRoot,      // ✅ Expected output
    pub da_block_header: Da::BlockHeader, // ✅ Block metadata
    pub relevant_proofs: RelevantProofs,  // ✅ Transaction proofs
    pub relevant_blobs: RelevantBlobs,    // ✅ Transaction data
    pub witness: Witness,                 // ✅ Execution witness
}
```

**No changes needed** - use as-is for TEE input.

#### Proof Sender Interface

**File:** `crates/rollup-interface/src/state_machine/stf/proof_sender.rs`

```rust
#[async_trait]
pub trait ProofSender: Send + Sync {
    async fn publish_proof_blob_with_metadata(
        &self,
        serialized_proof: SerializedAggregatedProof,
    ) -> anyhow::Result<()>;
}
```

**Extension needed:** Add method for submitting to Security Council instead of DA.

#### Node Block Processing

**File:** `crates/full-node/sov-stf-runner/src/runner.rs`

The main node loop already processes blocks correctly:

```rust
async fn process_next_slot(&mut self, ...) {
    // 1. Fetch block from DA
    let filtered_block = self.sync_fetcher.get_block_at(height).await?;
    
    // 2. Execute STF
    let slot_result = self.stf.apply_slot(
        self.state_manager.get_state_root(),
        stf_pre_state,
        Default::default(),
        &filtered_block_header,
        relevant_blobs.as_iters(),
        ExecutionContext::Node,
    );
    
    // 3. Process results
    self.state_manager.process_stf_changes(...).await?;
    
    // NEW: 4. Send to TEE manager for batch proving
    // self.tee_manager.add_processed_block(...).await?;
}
```

**Extension needed:** Add hook to send processed blocks to TEE manager.

### 12.2 Components That Need to Be Built

| Component | Description | Location |
|-----------|-------------|----------|
| **TEE Adapter** | Interface to Google Confidential Space | `crates/adapters/tee/` |
| **TEE Manager** | Batch orchestration for TEE proving | `crates/full-node/sov-stf-runner/src/processes/tee_manager/` |
| **Security Council Client** | HTTP client for attestation submission | `crates/full-node/sov-stf-runner/src/processes/tee_manager/council_client.rs` |
| **L1 Bridge Module** | Track L2→L1 withdrawals, build merkle tree | `crates/module-system/module-implementations/sov-l1-bridge/` |
| **L1 Contracts** | RollupChain, MessageQueue, Gateways | Solidity contracts (separate repo) |

### 12.3 Reuse Summary

```
┌─────────────────────────────────────────────────────────────────────────────┐
│                           CODEBASE REUSE MAP                                │
├─────────────────────────────────────────────────────────────────────────────┤
│                                                                             │
│  REUSE AS-IS (no changes)                                                   │
│  ────────────────────────                                                   │
│  • StateTransitionWitness      → TEE input data                             │
│  • State root computation      → Same algorithm in/out of TEE               │
│  • DA layer integration        → Block data retrieval                       │
│  • Sequencer block building    → No changes needed                          │
│                                                                             │
│  EXTEND (add fields/methods)                                                │
│  ────────────────────────────                                               │
│  • AggregatedProofPublicData   → Add withdraw_root, message_queue_hash      │
│  • ProofSender trait           → Add council submission method              │
│  • Runner                      → Add TEE manager integration                │
│                                                                             │
│  ADAPT (same pattern, different impl)                                       │
│  ─────────────────────────────────────                                      │
│  • ZkProofManager              → TeeProofManager (same batching logic)      │
│  • Hyperlane ISM               → Council signature verification pattern    │
│                                                                             │
│  BUILD NEW                                                                  │
│  ─────────                                                                  │
│  • TEE Adapter (Google Confidential Space)                                  │
│  • Security Council client                                                  │
│  • L1 Bridge module (withdrawal tree)                                       │
│  • L1 Solidity contracts                                                    │
│                                                                             │
└─────────────────────────────────────────────────────────────────────────────┘
```

### 12.4 Key File Locations

| Purpose | File Path |
|---------|-----------|
| Aggregated proof structure | `crates/rollup-interface/src/state_machine/zk/aggregated_proof.rs` |
| ZK traits & witness | `crates/rollup-interface/src/state_machine/zk/mod.rs` |
| Proof manager pattern | `crates/full-node/sov-stf-runner/src/processes/zk_manager/mod.rs` |
| Proof sender interface | `crates/rollup-interface/src/state_machine/stf/proof_sender.rs` |
| Node runner main loop | `crates/full-node/sov-stf-runner/src/runner.rs` |
| State manager | `crates/full-node/sov-stf-runner/src/state_manager/mod.rs` |
| Batch size config | `crates/full-node/full-node-configs/src/runner.rs` |
| Hyperlane ISM (signature pattern) | `crates/module-system/hyperlane/src/ism.rs` |
| Merkle tree implementation | `crates/module-system/module-implementations/midnight-privacy/src/merkle.rs` |

---

## Appendix A: Data Structures

### A.1 BatchPublicDataV1 (Consensus-Critical)

```solidity
/// @notice The canonical public data structure committed by TEE and signed by council
/// @dev Version 1 of the protocol. All fields are consensus-critical.
struct BatchPublicDataV1 {
    uint32  version;                    // = 1
    uint64  layer2ChainId;              // Cross-chain replay protection
    bytes32 rollupId;                   // Domain separation across rollup instances
    uint64  batchIndex;                 // The batch being finalized
    
    // DA commitment
    uint64  daStartHeight;              // DA block range start (inclusive)
    uint64  daEndHeight;                // DA block range end (inclusive)
    bytes32 daCommitment;               // Merkle root of DA block hashes
    
    // Message queue progress
    uint256 lastProcessedQueueIndex;    // Last L1→L2 message index processed
    bytes32 messageQueueHash;           // Rolling hash at that index
    
    // State transition
    bytes32 prevStateRoot;              // Starting state
    bytes32 prevBatchHash;              // Parent batch hash
    bytes32 postStateRoot;              // Resulting state
    bytes32 batchHash;                  // This batch's hash
    
    // Withdrawals
    bytes32 withdrawRoot;               // Merkle root of L2→L1 messages
}

// Canonical encoding: abi.encode(BatchPublicDataV1)
// Canonical hash: poseidon2(abi.encode(BatchPublicDataV1))
```

### A.2 TEE Attestation (Off-Chain Structure)

```solidity
/// @notice Full attestation package sent from TEE to Security Council
/// @dev This structure is used OFF-CHAIN. L1 contract only sees BatchPublicDataV1.
struct TEEAttestationPackage {
    // The public data (submitted to L1)
    BatchPublicDataV1 batchPublicData;
    
    // AMD SEV-SNP attestation (verified off-chain by council)
    bytes sevSnpReport;                 // Raw AMD SEV-SNP attestation report
    bytes certificateChain;             // AMD → Google certificate chain
    
    // Extracted fields (for convenience, derived from sevSnpReport)
    bytes32 measurement;                // VM image hash (from report)
    bytes64 reportData;                 // Should equal [H(batchPublicData), 0x00..00]
    uint64 guestSvn;                    // Guest security version
    uint64 tcbVersion;                  // TCB version
}

/// @notice Council verification extracts these fields from SEV-SNP report
struct SEVSNPReportFields {
    bytes32 measurement;                // VM image hash (384-bit truncated to 256)
    bytes64 reportData;                 // Custom data field
    uint64 guestSvn;                    // Guest security version number
    uint64 policy;                      // VM policy flags
    bytes chipId;                       // Unique chip identifier
    bytes signature;                    // AMD signature over report
}
```

### A.3 Council Signature Bundle

```solidity
/// @notice Aggregated council signatures submitted to L1
struct CouncilSignatureBundle {
    bytes[] signatures;                 // ECDSA signatures (65 bytes each)
    uint256 signerBitmap;              // Bitmap: bit i = councilMemberList[i] signed
}

/// @notice What council members sign
/// @dev councilDigest = poseidon2(abi.encode(
///     COUNCIL_DOMAIN_SEP,
///     layer2ChainId,
///     rollupId,
///     batchIndex,
///     councilEpoch,
///     rollupChainAddress,
///     bpdHash
/// ))
bytes32 constant COUNCIL_DOMAIN_SEP = poseidon2("TEE_ROLLUP_COUNCIL_ATTESTATION_V1");
```

### A.4 Attestation Freshness and Replay Rules

```solidity
/// @notice Rules to prevent attestation replay attacks
/// 
/// 1. BATCH INDEX MONOTONICITY
///    - L1 enforces: batchIndex == lastFinalizedBatchIndex + 1
///    - Prevents replaying old batches
///
/// 2. COUNCIL EPOCH
///    - councilDigest includes councilEpoch
///    - Incremented on council membership changes
///    - Prevents old council signatures from being valid
///
/// 3. STATE CONTINUITY
///    - prevStateRoot must match finalizedStateRoots[batchIndex - 1]
///    - prevBatchHash must match committedBatches[batchIndex - 1]
///    - Prevents forking the state
///
/// 4. MESSAGE QUEUE PROGRESS
///    - lastProcessedQueueIndex must be >= current unfinalized index
///    - Prevents skipping or replaying L1 messages
///
/// 5. OFF-CHAIN FRESHNESS (Council Policy)
///    - Council may reject attestations older than X minutes
///    - Council may require fresh quote for each submission
///    - These are policy decisions, not consensus rules
```

---

## Appendix B: Gas Costs

| Operation | Estimated Gas |
|-----------|--------------|
| Commit batch | ~50,000 |
| Finalize batch | ~100,000 |
| L1 → L2 deposit | ~80,000 |
| L2 → L1 withdrawal claim | ~100,000 |
| Council signature verification | ~3,000 per sig |

---

## Appendix C: Timing Parameters

| Parameter | Suggested Value | Purpose |
|-----------|----------------|---------|
| Block time (L2) | 2 seconds | Transaction throughput |
| Batch interval | 10-20 blocks | Amortize L1 costs |
| TEE execution timeout | 60 seconds | Liveness |
| Council verification timeout | 5 minutes | Finality |
| Forced inclusion delay | 24 hours | Anti-censorship |
| Withdrawal challenge period | N/A (TEE, not fraud proof) | - |

---

## Appendix D: Contract Interfaces

### D.1 IRollupChain

```solidity
interface IRollupChain {
    // Events
    event CommitBatch(uint256 indexed batchIndex, bytes32 indexed batchHash);
    event FinalizeBatch(
        uint256 indexed batchIndex,
        bytes32 indexed batchHash,
        bytes32 stateRoot,
        bytes32 withdrawRoot
    );

    // Views
    function lastFinalizedBatchIndex() external view returns (uint256);
    function committedBatches(uint256 batchIndex) external view returns (bytes32);
    function finalizedStateRoots(uint256 batchIndex) external view returns (bytes32);
    function withdrawRoots(uint256 batchIndex) external view returns (bytes32);
    function isBatchFinalized(uint256 batchIndex) external view returns (bool);
    function councilEpoch() external view returns (uint64);
    function rollupId() external view returns (bytes32);

    // Mutations
    function commitBatch(bytes32 parentBatchHash, bytes32 batchHash) external;
    
    /// @notice Finalize a batch with council signatures over BatchPublicDataV1
    /// @param batchPublicDataAbi ABI-encoded BatchPublicDataV1 struct
    /// @param signatures Council member signatures over councilDigest(...)
    /// @param signerBitmap Bitmap indicating which council members signed
    function finalizeBatch(
        bytes calldata batchPublicDataAbi,
        bytes[] calldata signatures,
        uint256 signerBitmap
    ) external;
}
```

#### D.1.1 finalizeBatch Verification Logic

```solidity
function finalizeBatch(
    bytes calldata batchPublicDataAbi,
    bytes[] calldata signatures,
    uint256 signerBitmap
) external {
    // 1. Decode the full BatchPublicDataV1
    BatchPublicDataV1 memory bpd = abi.decode(batchPublicDataAbi, (BatchPublicDataV1));
    
    // 2. Verify version
    require(bpd.version == 1, "INVALID_VERSION");
    
    // 3. Verify chain/rollup identity
    require(bpd.layer2ChainId == layer2ChainId, "WRONG_CHAIN");
    require(bpd.rollupId == rollupId, "WRONG_ROLLUP");
    
    // 4. Verify batch index is next expected
    require(bpd.batchIndex == lastFinalizedBatchIndex + 1, "WRONG_BATCH_INDEX");
    
    // 5. Verify state continuity
    require(
        bpd.prevStateRoot == finalizedStateRoots[lastFinalizedBatchIndex],
        "BAD_PREV_STATE_ROOT"
    );
    require(
        bpd.prevBatchHash == committedBatches[lastFinalizedBatchIndex],
        "BAD_PREV_BATCH_HASH"
    );
    
    // 6. Verify batch was committed
    require(
        bpd.batchHash == committedBatches[bpd.batchIndex],
        "BATCH_NOT_COMMITTED"
    );
    
    // 7. Verify message queue progress
    require(
        bpd.messageQueueHash == 
            IL1MessageQueue(messageQueue).getMessageRollingHash(bpd.lastProcessedQueueIndex),
        "BAD_MESSAGE_QUEUE_HASH"
    );
    
    // 8. Compute hashes and verify council signatures
    bytes32 bpdHash = poseidon2(batchPublicDataAbi);
    bytes32 digest = councilDigest(
        bpdHash,
        bpd.layer2ChainId,
        bpd.rollupId,
        bpd.batchIndex,
        councilEpoch,
        address(this)
    );
    
    require(
        verifyThresholdSignatures(digest, signatures, signerBitmap),
        "INSUFFICIENT_SIGNATURES"
    );
    
    // 9. Store results
    finalizedStateRoots[bpd.batchIndex] = bpd.postStateRoot;
    withdrawRoots[bpd.batchIndex] = bpd.withdrawRoot;
    lastFinalizedBatchIndex = bpd.batchIndex;
    
    // 10. Advance message queue
    IL1MessageQueue(messageQueue).finalizePoppedCrossDomainMessage(
        bpd.lastProcessedQueueIndex + 1
    );
    
    emit FinalizeBatch(bpd.batchIndex, bpd.batchHash, bpd.postStateRoot, bpd.withdrawRoot);
}
```

### D.2 IL1MessageQueue

```solidity
interface IL1MessageQueue {
    function nextCrossDomainMessageIndex() external view returns (uint256);
    function getMessageRollingHash(uint256 index) external view returns (bytes32);
    function appendCrossDomainMessage(
        address target,
        uint256 gasLimit,
        bytes calldata data
    ) external;
    function finalizePoppedCrossDomainMessage(uint256 newIndex) external;
}
```

---

---

*Document Version: 1.3*
*Last Updated: December 2025*
*Changes in 1.3: Updated hash function from keccak256 to poseidon2.*
*Changes in 1.2: Updated all references from Ethereum/ETH to Midnight/NIGHT.*
*Changes in 1.1: Added BatchPublicDataV1 with full field specification, domain-separated council signatures, precise DA commitment algorithm, index-based message queue verification, explicit on-chain vs off-chain verification boundaries, AMD SEV-SNP scoping.*

