# Selective Disclosure in the Privacy Pool

This document explains how selective disclosure works in the note-spend circuit, designed for non-developers to understand the concepts.

---

## What is Selective Disclosure?

Imagine you have a sealed envelope containing your bank statement. With selective disclosure, you can prove things about what's inside (like "I have enough money") without ever opening the envelope. Even better, you can give certain trusted people a special key to peek inside, while everyone else just sees the sealed envelope.

In our privacy pool system, this means:
- **Your transaction details stay private** (amounts, who sent, who receives)
- **The system can still verify everything is valid** (no cheating, no double-spending)
- **A mandatory auditor can always see details** (regulatory compliance)
- **The receiver can see their note details** (so they can spend it later)

---

## The Four Parties in the Privacy Pool

```
╔═══════════════════════════════════════════════════════════════════════════════╗
║                           FOUR DIFFERENT PARTIES                              ║
╠═══════════════════════════════════════════════════════════════════════════════╣
║                                                                               ║
║   SENDER              SEQUENCER/NODE         AUDITOR            RECEIVER      ║
║   (creates tx)        (validates tx)         (mandatory)        (gets funds)  ║
║                                                                               ║
║   Knows:              Knows:                 Knows:             Knows:        ║
║   • Their own tx's    • Public inputs        • Their FVK        • Their keys  ║
║     private data      • The proof            • Public inputs    • Public      ║
║   • Auditor's         • Auditor's            • All ciphertexts    inputs      ║
║     fvk_commit          fvk_commit                              • Note details║
║   • Receiver's        • NO viewing keys                           (to spend)  ║
║     address                                                                   ║
║                                                                               ║
║   Can:                Can:                   Can:               Can:          ║
║   • Create proof      • Verify proof         • Decrypt ALL      • Receive     ║
║   • Publish           • Check hashes           transactions       funds       ║
║     ciphertexts       • ENFORCE auditor      • See every        • Spend the   ║
║                         fvk_commit present     value, sender,     note later  ║
║                       • Accept/reject tx       recipient        • See their   ║
║                                                                   own tx data ║
║                                                                               ║
║   ❌ Can't:           ❌ Can't:              ❌ Can't:          ❌ Can't:      ║
║   • Fake the proof    • Decrypt anything     • Spend funds      • See other   ║
║   • Skip auditor      • Know what's inside   • Change anything    users' txs  ║
║     attestation       • Accept tx without    • Create proofs    • See auditor ║
║                         auditor attestation                       view        ║
║                                                                               ║
╚═══════════════════════════════════════════════════════════════════════════════╝
```

---

## The Mandatory Auditor Model

The protocol enforces that every transaction includes an attestation for a designated auditor. However, since FVK is a symmetric key, we can't use a single shared key (everyone could decrypt everyone else's transactions). Instead, we use a signature-based distribution model:

```
╔═══════════════════════════════════════════════════════════════════════════════╗
║                    AUDITOR KEY DISTRIBUTION MODEL                             ║
╠═══════════════════════════════════════════════════════════════════════════════╣
║                                                                               ║
║   THE PROBLEM WITH A SINGLE SHARED FVK:                                       ║
║   ─────────────────────────────────────                                       ║
║   FVK is symmetric → If all users had the SAME fvk, they could                ║
║   decrypt each other's transactions. That defeats privacy!                    ║
║                                                                               ║
║   THE SOLUTION: Signed unique FVKs                                            ║
║   ─────────────────────────────────────                                       ║
║                                                                               ║
║   AUDITOR SERVICE                                                             ║
║   ┌─────────────────────────────────────────────────────────────────────┐     ║
║   │  Has:                                                               │     ║
║   │  • Signing private key (auditor_sk)                                 │     ║
║   │  • Database of all issued FVKs (can decrypt everything)             │     ║
║   └─────────────────────────────────────────────────────────────────────┘     ║
║                           │                                                   ║
║                           │ User requests an FVK                              ║
║                           ▼                                                   ║
║   ┌─────────────────────────────────────────────────────────────────────┐     ║
║   │  Auditor generates:                                                 │     ║
║   │  • fvk = random 32 bytes (UNIQUE per user or per request)           │     ║
║   │  • fvk_commit = H("FVK_COMMIT_V1" || fvk)                           │     ║
║   │  • signature = Sign(auditor_sk, fvk_commit)                         │     ║
║   │                                    └───────────┘                    │     ║
║   │                                    Signature is ONLY on fvk_commit  │     ║
║   │                                    (the hash, not the secret fvk)   │     ║
║   │                                                                     │     ║
║   │  Returns to user:                                                   │     ║
║   │  ┌─────────────────────────────────────────────────────────────┐    │     ║
║   │  │  PRIVATE (only user knows):     PUBLIC (can be shared):     │    │     ║
║   │  │  • fvk                          • fvk_commit                │    │     ║
║   │  │    (the secret key)             • signature                 │    │     ║
║   │  │                                   (on fvk_commit only)      │    │     ║
║   │  └─────────────────────────────────────────────────────────────┘    │     ║
║   │                                                                     │     ║
║   │  Stores internally: fvk (to decrypt later)                          │     ║
║   └─────────────────────────────────────────────────────────────────────┘     ║
║                           │                                                   ║
║                           ▼                                                   ║
║   USER                                                                        ║
║   ┌─────────────────────────────────────────────────────────────────────┐     ║
║   │  Uses in ZK proof:   fvk (PRIVATE input)                            │     ║
║   │  Publishes on-chain: fvk_commit + signature (PUBLIC)                │     ║
║   │                                                                     │     ║
║   │  The signature proves fvk_commit came from the auditor              │     ║
║   │  WITHOUT revealing the secret fvk!                                  │     ║
║   │                                                                     │     ║
║   │  User can reuse the same fvk for multiple txs, or request new ones  │     ║
║   └─────────────────────────────────────────────────────────────────────┘     ║
║                           │                                                   ║
║                           │ Transaction includes: fvk_commit + signature      ║
║                           ▼                                                   ║
║   SEQUENCER/NODE                                                              ║
║   ┌─────────────────────────────────────────────────────────────────────┐     ║
║   │  Has:                                                               │     ║
║   │  • Auditor's PUBLIC signing key (auditor_pk) - HARDCODED            │     ║
║   │                                                                     │     ║
║   │  Validates:                                                         │     ║
║   │  1. ✓ ZK proof is valid                                             │     ║
║   │  2. ✓ Verify(auditor_pk, signature, fvk_commit)                     │     ║
║   │       → Proves this fvk_commit was issued by the real auditor       │     ║
║   │       → Does NOT reveal the secret fvk                              │     ║
║   │  3. ✓ ct_hash matches published ciphertext                          │     ║
║   │                                                                     │     ║
║   │  If ANY check fails → REJECT TRANSACTION                            │     ║
║   │                                                                     │     ║
║   │  Does NOT need: the actual fvk                                      │     ║
║   └─────────────────────────────────────────────────────────────────────┘     ║
║                                                                               ║
╚═══════════════════════════════════════════════════════════════════════════════╝

WHY THE SIGNATURE IS ON fvk_commit (NOT fvk):
══════════════════════════════════════════════════════════════════════════════════

┌─────────────────────────────────────────────────────────────────────────────┐
│                                                                             │
│   fvk (secret)  ────hash────▶  fvk_commit (public)  ◄────signed by auditor  │
│        │                              │                                     │
│        │                              │                                     │
│   Only known by:                 Can be shared:                             │
│   • User (to encrypt)            • On-chain in transaction                  │
│   • Auditor (to decrypt)         • Signature proves auditor issued it       │
│                                  • Anyone can verify without learning fvk   │
│                                                                             │
└─────────────────────────────────────────────────────────────────────────────┘

This means:
• Sender CANNOT create a valid tx without an auditor-signed fvk_commit
• Every shielded transaction is readable by the auditor (who stores all fvks)
• Users CANNOT decrypt each other's transactions (each has unique fvk)
• Privacy is preserved from PUBLIC, but auditor always has access
```

---

## The Privacy Layers

```
╔═══════════════════════════════════════════════════════════════════════════╗
║                    WHO CAN SEE WHAT                                        ║
╠═══════════════════════════════════════════════════════════════════════════╣
║                                                                           ║
║                        Transaction Details                                ║
║                 (value, sender, recipient, etc.)                          ║
║                                                                           ║
║   ┌─────────────────────────────────────────────────────────────────┐     ║
║   │                                                                 │     ║
║   │   PUBLIC            AUDITOR           RECEIVER          SENDER  │     ║
║   │   (everyone)        (mandatory)       (of funds)        (you)   │     ║
║   │                                                                 │     ║
║   │      ❌                 ✅               ✅                ✅   │     ║
║   │   Can't see         Sees ALL         See THEIR        See THEIR │     ║
║   │   details           transactions     own notes        own txs   │     ║
║   │                                                                 │     ║
║   └─────────────────────────────────────────────────────────────────┘     ║
║                                                                           ║
║   ─────────────────────────────────────────────────────────────────────   ║
║                                                                           ║
║   PRIVACY MODEL:                                                          ║
║                                                                           ║
║   • Private FROM the public blockchain observers                          ║
║   • Private FROM other users                                              ║
║   • NOT private FROM the auditor (by design)                              ║
║   • Receiver sees their own incoming notes                                ║
║                                                                           ║
╚═══════════════════════════════════════════════════════════════════════════╝
```

---

## How a Note Commitment Hides Your Data

A **note** is like a digital banknote. It stores who owns it and how much it's worth. But instead of publishing this openly, we create a **commitment** — a cryptographic fingerprint that hides the details.

```
    YOUR PRIVATE NOTE DATA                         WHAT THE WORLD SEES
    ──────────────────────                         ────────────────────

    ┌────────────────────┐                         ┌────────────────────┐
    │ Domain: "myapp"    │                         │                    │
    │ Value:  100 tokens │                         │    Commitment      │
    │ Rho:    [random]   │  ────────────────────▶  │                    │
    │ Recipient: 0xABC.. │       Hash Function     │  0x7f3a2b...c8d9   │
    │ Sender: 0x123..    │    (one-way, secure)    │                    │
    └────────────────────┘                         └────────────────────┘

              │                                              │
              │                                              │
              ▼                                              ▼
        "I know Alice                               "Someone created
         sent 100 to Bob"                            a new note"
```

The hash function (Poseidon2 in our system) is like a blender — it mixes all the ingredients into a smoothie. You can verify the smoothie came from specific ingredients if you have them, but you can't reverse-engineer the recipe just by tasting it.

---

## How the Receiver Gets Note Information

```
┌─────────────────────────────────────────────────────────────────────┐
│                    RECEIVER'S PERSPECTIVE                           │
└─────────────────────────────────────────────────────────────────────┘

The receiver needs to know the note details to SPEND it later:
• value (how much is in the note)
• rho (random identifier needed for nullifier)
• their position in the Merkle tree

Two ways the receiver can learn this:

┌─────────────────────────────────────────────────────────────────────┐
│  OPTION A: Out-of-band communication                                │
│  ─────────────────────────────────────────────────────────────────  │
│  Sender tells receiver directly: "I sent you 100 tokens,            │
│  here's the rho, here's the position..."                            │
│                                                                     │
│  ✓ Simple                                                           │
│  ✗ Requires sender-receiver coordination                            │
│  ✗ What if sender disappears?                                       │
└─────────────────────────────────────────────────────────────────────┘

┌─────────────────────────────────────────────────────────────────────┐
│  OPTION B: Incoming View Key (pk_ivk) — what our system uses        │
│  ─────────────────────────────────────────────────────────────────  │
│                                                                     │
│  Receiver has an "incoming view key" that lets them:                │
│  • Scan the chain for notes addressed to them                       │
│  • Decrypt the note details from on-chain data                      │
│                                                                     │
│  From the note commitment structure (NOTE_V2):                      │
│    recipient = H("ADDR_V2" || domain || pk_spend || pk_ivk)         │
│                                                   └──────┘          │
│                                                   Receiver's        │
│                                                   incoming view key │
│                                                                     │
│  ✓ Receiver can independently find their notes                      │
│  ✓ No coordination needed after sender creates tx                   │
│  ✓ Works even if sender goes offline                                │
└─────────────────────────────────────────────────────────────────────┘
```

---

## Transaction Structure with Mandatory Auditor

```
TRANSACTION WITH MANDATORY AUDITOR
═══════════════════════════════════════════════════════════════════════

┌─────────────────────────────────────────────────────────────────────┐
│  CORE TRANSACTION:                                                  │
│  • anchor, nullifiers, cm_out, withdraw_amount, withdraw_to         │
└─────────────────────────────────────────────────────────────────────┘

┌─────────────────────────────────────────────────────────────────────┐
│  VIEWER ATTESTATIONS:                                               │
│  ─────────────────────────────────────────────────────────────────  │
│                                                                     │
│  n_viewers: 2  (example: 1 auditor + 1 optional viewer)             │
│                                                                     │
│  ┌───────────────────────────────────────────────────────────────┐  │
│  │  ATTESTATION #0: AUDITOR (MANDATORY)                          │  │
│  │  ─────────────────────────────────────────────────────────────│  │
│  │  fvk_commit: 0x7a3b9c... ◄── Must match AUDITOR_FVK_COMMIT    │  │
│  │  ct_hash:    0x...                                            │  │
│  │  mac:        0x...                                            │  │
│  │  ciphertext: [144 bytes]                                      │  │
│  │                                                               │  │
│  │  ⚠️ NODE ENFORCES: This MUST be present and match!            │  │
│  └───────────────────────────────────────────────────────────────┘  │
│                                                                     │
│  ┌───────────────────────────────────────────────────────────────┐  │
│  │  ATTESTATION #1: OPTIONAL VIEWER                              │  │
│  │  ─────────────────────────────────────────────────────────────│  │
│  │  fvk_commit: 0xabc123... (sender's accountant, maybe)         │  │
│  │  ct_hash:    0x...                                            │  │
│  │  mac:        0x...                                            │  │
│  │  ciphertext: [144 bytes]                                      │  │
│  └───────────────────────────────────────────────────────────────┘  │
│                                                                     │
└─────────────────────────────────────────────────────────────────────┘
```

---

## Complete Transaction Flow

```
SENDER                SEQUENCER              AUDITOR           RECEIVER
══════                ═════════              ═══════           ════════

1. Build tx with:
   • Auditor's fvk_commit (REQUIRED)
   • Receiver's address
          │
          ▼
┌─────────────────┐
│ Create ZK proof │
│ with attestation│
│ for auditor     │
└────────┬────────┘
          │
          │ Submit tx
          │
          ▼
                      ┌────────────────────────┐
                      │ 2. Validate:           │
                      │                        │
                      │ ✓ Proof valid          │
                      │                        │
                      │ ✓ Attestation[0] has   │
                      │   fvk_commit ==        │
                      │   AUDITOR_FVK_COMMIT   │
                      │                        │
                      │ ✓ All ct_hash match    │
                      │                        │
                      │ If pass → ACCEPT       │
                      │ If fail → REJECT       │
                      └───────────┬────────────┘
                                  │
                                  │ On-chain
                                  │
                                  ▼
                                               ┌──────────────────┐
                                               │ 3. Auditor scans │
                                               │    ALL txs       │
                                               │                  │
                                               │ For each tx:     │
                                               │ • Find their     │
                                               │   attestation    │
                                               │ • Decrypt        │
                                               │ • Log/analyze    │
                                               │                  │
                                               │ Has FULL         │
                                               │ visibility!      │
                                               └──────────────────┘

                                                                ┌──────────────┐
                                                                │ 4. Receiver  │
                                                                │    scans txs │
                                                                │              │
                                                                │ Uses their   │
                                                                │ incoming     │
                                                                │ view key to  │
                                                                │ find & decode│
                                                                │ their notes  │
                                                                │              │
                                                                │ Can now      │
                                                                │ SPEND the    │
                                                                │ note!        │
                                                                └──────────────┘
```

---

## Why This Design?

```
┌─────────────────────────────────────────────────────────────────────┐
│                    REGULATORY COMPLIANCE + PRIVACY                   │
└─────────────────────────────────────────────────────────────────────┘

The mandatory auditor model achieves BOTH:

┌─────────────────────────────────────────────────────────────────────┐
│                                                                     │
│   1. PRIVACY FOR USERS                                              │
│      ───────────────────                                            │
│      • Public observers see nothing (just nullifiers/commitments)   │
│      • Other users can't spy on your transactions                   │
│      • Your financial activity is shielded from mass surveillance   │
│                                                                     │
│   2. AUDITABILITY FOR COMPLIANCE                                    │
│      ───────────────────────────                                    │
│      • Designated auditor can review all transactions               │
│      • Enables regulatory compliance (AML/KYC if needed)            │
│      • Auditor can investigate suspicious activity                  │
│      • Provides legal liability protection for the protocol         │
│                                                                     │
│   3. RECEIVER ACCESS                                                │
│      ───────────────────                                            │
│      • Receiver can find and decode their incoming notes            │
│      • Uses incoming view key (pk_ivk) to scan the chain            │
│      • Can spend received funds without sender coordination         │
│                                                                     │
└─────────────────────────────────────────────────────────────────────┘
```

---

# Deep Dive: The FVK (Full Viewing Key) System

## What is an FVK?

The **Full Viewing Key (FVK)** is a 32-byte secret that the auditor (or any viewer) possesses. Think of it like a "read-only password" — it lets them decrypt and read transaction details, but they can't spend anything.

```
╔═══════════════════════════════════════════════════════════════════════╗
║                        FVK LIFECYCLE                                   ║
╠═══════════════════════════════════════════════════════════════════════╣
║                                                                       ║
║   1. AUDITOR creates their FVK (off-chain)                            ║
║      ┌──────────────────────────────────────┐                         ║
║      │  fvk = random 32 bytes               │                         ║
║      │  (auditor keeps this SECRET)         │                         ║
║      └──────────────────────────────────────┘                         ║
║                        │                                              ║
║                        ▼                                              ║
║   2. AUDITOR computes FVK commitment and shares it publicly           ║
║      ┌──────────────────────────────────────┐                         ║
║      │  fvk_commit = H("FVK_COMMIT_V1" ||   │                         ║
║      │                  fvk)                │                         ║
║      │                                      │                         ║
║      │  (this is PUBLIC — like a mailbox    │                         ║
║      │   address, not the key to open it)   │                         ║
║      └──────────────────────────────────────┘                         ║
║                        │                                              ║
║                        ▼                                              ║
║   3. Protocol HARDCODES this fvk_commit as mandatory                  ║
║      All nodes enforce its presence in every transaction              ║
║                        │                                              ║
║                        ▼                                              ║
║   4. AUDITOR uses their fvk to decrypt all on-chain ciphertexts       ║
║                                                                       ║
╚═══════════════════════════════════════════════════════════════════════╝
```

---

## The Encryption Flow (What Our Circuit Actually Does)

Here's the exact process from our code:

```
┌─────────────────────────────────────────────────────────────────────────┐
│                    VIEWER ATTESTATION FLOW                               │
│                    (Per viewer, per output note)                         │
└─────────────────────────────────────────────────────────────────────────┘

INPUTS TO THE CIRCUIT:
─────────────────────────────────────────────────────────────────────────

PUBLIC (everyone sees):                PRIVATE (only sender knows):
┌────────────────────────┐            ┌────────────────────────┐
│ fvk_commit_arg         │            │ fvk (auditor's key)    │
│ ct_hash_arg            │            │ note plaintext:        │
│ mac_arg                │            │   - domain             │
│ cm (note commitment)   │            │   - value              │
└────────────────────────┘            │   - rho                │
                                      │   - recipient          │
                                      │   - sender_id          │
                                      └────────────────────────┘

WHAT THE CIRCUIT COMPUTES & VERIFIES:
─────────────────────────────────────────────────────────────────────────

Step 1: Verify the FVK matches the public commitment
┌─────────────────────────────────────────────────────────────────────┐
│                                                                     │
│   computed_fvk_commit = H("FVK_COMMIT_V1" || fvk)                   │
│                                                                     │
│   ASSERT: computed_fvk_commit == fvk_commit_arg  ✓                  │
│                                                                     │
│   → This ensures sender used the CORRECT auditor key                │
│   → Sender can't claim to encrypt for auditor but use different key │
│                                                                     │
└─────────────────────────────────────────────────────────────────────┘
                                 │
                                 ▼
Step 2: Derive encryption key from FVK + note commitment
┌─────────────────────────────────────────────────────────────────────┐
│                                                                     │
│   k = H("VIEW_KDF_V1" || fvk || cm)                                 │
│                                                                     │
│   → Each note gets a UNIQUE encryption key                          │
│   → Even same auditor, different notes = different keys             │
│   → Prevents cross-note attacks                                     │
│                                                                     │
└─────────────────────────────────────────────────────────────────────┘
                                 │
                                 ▼
Step 3: Encrypt the plaintext using stream cipher
┌─────────────────────────────────────────────────────────────────────┐
│                                                                     │
│   plaintext (144 bytes) = [ domain | value | rho | recipient |     │
│                             sender_id ]                             │
│                                                                     │
│   keystream[i] = H("VIEW_STREAM_V1" || k || counter_i)             │
│                                                                     │
│   ciphertext = plaintext XOR keystream                              │
│                                                                     │
│   → Uses 5 hash blocks (32 bytes each) for 144 bytes                │
│                                                                     │
└─────────────────────────────────────────────────────────────────────┘
                                 │
                                 ▼
Step 4: Compute ciphertext hash
┌─────────────────────────────────────────────────────────────────────┐
│                                                                     │
│   ct_hash = H("CT_HASH_V1" || ciphertext)                          │
│                                                                     │
│   ASSERT: ct_hash == ct_hash_arg  ✓                                 │
│                                                                     │
│   → This binds the EXACT ciphertext to the proof                    │
│   → Sender can't publish different ciphertext than what was proved  │
│                                                                     │
└─────────────────────────────────────────────────────────────────────┘
                                 │
                                 ▼
Step 5: Compute and verify MAC (Message Authentication Code)
┌─────────────────────────────────────────────────────────────────────┐
│                                                                     │
│   mac = H("VIEW_MAC_V1" || k || cm || ct_hash)                     │
│                                                                     │
│   ASSERT: mac == mac_arg  ✓                                         │
│                                                                     │
│   → Proves ciphertext wasn't tampered with                          │
│   → Links ciphertext to the specific note commitment                │
│   → Auditor can verify they decrypted correctly                     │
│                                                                     │
└─────────────────────────────────────────────────────────────────────┘
```

---

## Who Validates What? The Sequencer vs. Auditor

The sequencer doesn't need the FVK to ensure the transaction is valid and decryptable:

```
┌─────────────────────────────────────────────────────────────────────┐
│                  SEQUENCER VERIFICATION                             │
└─────────────────────────────────────────────────────────────────────┘

The sequencer does NOT need the FVK to verify these things:

1. ✓ PROOF IS VALID
   └── ZK verification: "All constraints in the circuit were satisfied"
   └── This guarantees the sender couldn't lie about the data

2. ✓ AUDITOR ATTESTATION PRESENT
   └── Check: fvk_commit in attestation[0] == AUDITOR_FVK_COMMIT
   └── No decryption needed, just comparing public values

3. ✓ CIPHERTEXT HASH MATCHES (simple hash check, no decryption!)
   
   ┌─────────────────────────────────────────────────────────────────┐
   │                                                                 │
   │   published_ciphertext ────▶ H("CT_HASH_V1" || ct) ────┐       │
   │                                                         │       │
   │                                      compare ◄──────────┤       │
   │                                         │               │       │
   │   ct_hash_arg (from proof) ─────────────┘               │       │
   │                                                         │       │
   │   If they match → ciphertext is authentic               │       │
   │   If they don't → REJECT TRANSACTION                    │       │
   │                                                                 │
   └─────────────────────────────────────────────────────────────────┘

The sequencer can enforce: "The ciphertext you published MUST hash to
the ct_hash that was proven in the ZK proof."

This is just a hash comparison — no secret keys needed!
```

---

## Security Questions: What CAN'T a Sender Do?

### ❌ Can the sender encrypt WRONG information?

**No.** Here's why:

```
┌─────────────────────────────────────────────────────────────────────┐
│  THE PLAINTEXT IS DERIVED FROM THE SAME DATA AS THE COMMITMENT      │
└─────────────────────────────────────────────────────────────────────┘

The note commitment that goes on-chain:

    cm = H("NOTE_V2" || domain || value || rho || recipient || sender_id)
          └──────────────────────────────────────────────────────────┘
                                These values

The plaintext encrypted for auditor:

    plaintext = [ domain || value || rho || recipient || sender_id ]
                └─────────────────────────────────────────────────────┘
                                SAME values!

─────────────────────────────────────────────────────────────────────

From our code (lines 998-1008):

    for j in 0..n_out {
        encode_note_plain(
            &domain,          // ← same domain
            outs[j].v,        // ← same value used in note_commitment
            &outs[j].rho,     // ← same rho used in note_commitment  
            &outs[j].rcp,     // ← same recipient used in note_commitment
            &sender_id,       // ← same sender_id
            &mut out_pts[j],
        );
    }

The circuit uses the EXACT SAME variables for both:
- Creating the note commitment (verified against public cm)
- Encrypting the plaintext for auditor

═══════════════════════════════════════════════════════════════════════
RESULT: If the sender tries to encrypt fake data, either:

1. The note commitment check fails (wrong cm), OR
2. The auditor attestation matches the REAL commitment

There's no way to have a valid proof with mismatched data!
═══════════════════════════════════════════════════════════════════════
```

### ❌ Can the sender encrypt for the WRONG auditor?

**No.** The FVK commitment is verified:

```
┌─────────────────────────────────────────────────────────────────────┐
│  FVK COMMITMENT CHECK (lines 1020-1021)                             │
└─────────────────────────────────────────────────────────────────────┘

PUBLIC INPUT:   fvk_commit_arg  (the auditor's published commitment)
PRIVATE INPUT:  fvk             (the actual key used for encryption)

Circuit computes:  computed = H("FVK_COMMIT_V1" || fvk)
Circuit asserts:   computed == fvk_commit_arg

─────────────────────────────────────────────────────────────────────

SCENARIO: Sender tries to use wrong key

    Auditor's commitment:  AUDITOR_FVK_COMMIT = H("FVK_COMMIT_V1" || fvk_auditor)
    
    Sender tries to use:   fvk_fake (different key)
    
    Circuit computes:      H("FVK_COMMIT_V1" || fvk_fake)
                                     ↓
                            ≠ AUDITOR_FVK_COMMIT
                                     ↓
                              PROOF FAILS! ✗

═══════════════════════════════════════════════════════════════════════
RESULT: The sender MUST use the key that corresponds to the hardcoded
AUDITOR_FVK_COMMIT. They can't substitute a different key.
═══════════════════════════════════════════════════════════════════════
```

### ❌ Can the sender create ciphertext that can't be decrypted?

**No.** The ct_hash binds the exact ciphertext, and the sequencer verifies it:

```
┌─────────────────────────────────────────────────────────────────────┐
│  CIPHERTEXT HASH CHECK                                              │
└─────────────────────────────────────────────────────────────────────┘

The circuit:
1. Encrypts the plaintext → produces ciphertext in ct_buf
2. Computes ct_hash = H("CT_HASH_V1" || ct_buf)
3. ASSERTS ct_hash == ct_hash_arg (public input)

The sequencer:
1. Receives the published ciphertext alongside the transaction
2. Computes H("CT_HASH_V1" || published_ciphertext)
3. Compares with ct_hash_arg from the proof
4. If they don't match → REJECT TRANSACTION

═══════════════════════════════════════════════════════════════════════
RESULT: The sender MUST publish the exact ciphertext that was proven.
Publishing garbage would fail the hash check at the sequencer.
The sequencer does NOT need the FVK to do this check!
═══════════════════════════════════════════════════════════════════════
```

### ❌ Can someone tamper with the ciphertext after the fact?

**No.** The MAC proves integrity:

```
┌─────────────────────────────────────────────────────────────────────┐
│  MAC VERIFICATION (lines 1029, 1035-1037)                           │
└─────────────────────────────────────────────────────────────────────┘

mac = H("VIEW_MAC_V1" || k || cm || ct_hash)
                        │    │     │
                        │    │     └── hash of the ciphertext
                        │    └── the note commitment (public)
                        └── derived from fvk (only auditor knows)

─────────────────────────────────────────────────────────────────────

When auditor decrypts:
1. Auditor derives k = H("VIEW_KDF_V1" || fvk || cm)
2. Auditor computes expected_mac = H("VIEW_MAC_V1" || k || cm || ct_hash)
3. Compares with mac_arg from the proof
4. If match → ciphertext is:
   a) Created by someone who knew fvk (or had a valid proof)
   b) Linked to this specific note commitment
   c) Not modified since creation

═══════════════════════════════════════════════════════════════════════
RESULT: Tampering with ciphertext would break the MAC check.
The auditor can detect any modifications.
═══════════════════════════════════════════════════════════════════════
```

---

## Visual Summary: The Trust Chain

```
╔═════════════════════════════════════════════════════════════════════════════╗
║                    VIEWER ATTESTATION TRUST CHAIN                            ║
╠═════════════════════════════════════════════════════════════════════════════╣
║                                                                             ║
║   ┌─────────────┐                                                           ║
║   │ Note Data   │  (value, rho, recipient, sender_id, domain)               ║
║   └──────┬──────┘                                                           ║
║          │                                                                  ║
║          ├─────────────────────────────┬────────────────────────────────┐   ║
║          │                             │                                │   ║
║          ▼                             ▼                                │   ║
║   ┌─────────────────┐           ┌─────────────────┐                     │   ║
║   │ Note Commitment │           │ Plaintext for   │                     │   ║
║   │ (PUBLIC)        │           │ Encryption      │                     │   ║
║   └────────┬────────┘           └────────┬────────┘                     │   ║
║            │                             │                              │   ║
║            │ Verified against            │ Encrypted with               │   ║
║            │ public cm argument          │ auditor's FVK                │   ║
║            ▼                             ▼                              │   ║
║   ┌─────────────────┐           ┌─────────────────┐                     │   ║
║   │ ✓ cm matches    │           │ Ciphertext      │                     │   ║
║   └─────────────────┘           └────────┬────────┘                     │   ║
║                                          │                              │   ║
║                                          ├─────────────────┐            │   ║
║                                          │                 │            │   ║
║                                          ▼                 ▼            │   ║
║                                   ┌───────────┐     ┌───────────┐       │   ║
║                                   │ ct_hash   │     │ MAC       │       │   ║
║                                   │ (PUBLIC)  │     │ (PUBLIC)  │       │   ║
║                                   └─────┬─────┘     └─────┬─────┘       │   ║
║                                         │                 │             │   ║
║                                         │ Verified        │ Verified    │   ║
║                                         ▼                 ▼             │   ║
║                                   ┌───────────┐     ┌───────────┐       │   ║
║                                   │ ✓ matches │     │ ✓ matches │       │   ║
║                                   └───────────┘     └───────────┘       │   ║
║                                                                         │   ║
║   ═══════════════════════════════════════════════════════════════════   │   ║
║                                                                         │   ║
║   WHAT THIS GUARANTEES:                                                 │   ║
║                                                                         │   ║
║   1. ✓ Same data in commitment AND encrypted plaintext (can't lie)     │   ║
║   2. ✓ Correct auditor key used (fvk_commit verified)                   │   ║
║   3. ✓ Ciphertext is exactly what was proven (ct_hash verified)        │   ║
║   4. ✓ Ciphertext hasn't been tampered with (MAC verified)             │   ║
║   5. ✓ Ciphertext is linked to this specific note (cm in MAC)          │   ║
║                                                                         │   ║
╚═════════════════════════════════════════════════════════════════════════════╝
```

---

## What the Auditor Actually Does (Off-Chain)

```
╔═══════════════════════════════════════════════════════════════════════╗
║                    AUDITOR DECRYPTION PROCESS                          ║
╠═══════════════════════════════════════════════════════════════════════╣
║                                                                       ║
║   Auditor receives from on-chain:                                     ║
║   ┌────────────────────────────────────────────────┐                  ║
║   │ • cm (note commitment)                         │                  ║
║   │ • fvk_commit_arg (should match their fvk)      │                  ║
║   │ • ct_hash_arg                                  │                  ║
║   │ • mac_arg                                      │                  ║
║   │ • ciphertext (144 bytes, published separately) │                  ║
║   └────────────────────────────────────────────────┘                  ║
║                                                                       ║
║   Auditor has:                                                        ║
║   ┌────────────────────────────────────────────────┐                  ║
║   │ • fvk (their secret viewing key)               │                  ║
║   └────────────────────────────────────────────────┘                  ║
║                                                                       ║
║   Decryption steps:                                                   ║
║   ─────────────────────────────────────────────────────────────────   ║
║                                                                       ║
║   1. Check: H("FVK_COMMIT_V1" || fvk) == fvk_commit_arg               ║
║      └── "Is this attestation for me?"                                ║
║                                                                       ║
║   2. Check: H("CT_HASH_V1" || ciphertext) == ct_hash_arg              ║
║      └── "Is the ciphertext authentic?"                               ║
║                                                                       ║
║   3. Derive: k = H("VIEW_KDF_V1" || fvk || cm)                        ║
║      └── Get the encryption key                                       ║
║                                                                       ║
║   4. Check: H("VIEW_MAC_V1" || k || cm || ct_hash_arg) == mac_arg     ║
║      └── "Was this created correctly with my key?"                    ║
║                                                                       ║
║   5. Decrypt: plaintext = ciphertext XOR keystream(k)                 ║
║      └── Get the actual note details!                                 ║
║                                                                       ║
║   6. Parse plaintext:                                                 ║
║      ┌──────────────────────────────────────────────┐                 ║
║      │ bytes  0-31:  domain                         │                 ║
║      │ bytes 32-47:  value (16-byte LE, u64 + pad)  │                 ║
║      │ bytes 48-79:  rho                            │                 ║
║      │ bytes 80-111: recipient                      │                 ║
║      │ bytes 112-143: sender_id                     │                 ║
║      └──────────────────────────────────────────────┘                 ║
║                                                                       ║
║   7. (Optional) Verify: H("NOTE_V2" || ...) == cm                     ║
║      └── Double-check the plaintext matches the commitment            ║
║                                                                       ║
╚═══════════════════════════════════════════════════════════════════════╝
```

---

## Summary Tables

### The Four-Party Model

| Party | Role | What They See | Mandatory? |
|-------|------|---------------|------------|
| **Sender** | Creates transaction | Their own tx details | Yes (it's their tx) |
| **Sequencer** | Validates & accepts | Public data only, enforces auditor | Yes |
| **Auditor** | Regulatory oversight | ALL transactions via attestation | Yes (protocol-enforced) |
| **Receiver** | Gets the funds | Their own notes via incoming view key | Yes (to spend later) |

### Attack Scenarios

| Attack | Can it work? | Why not? |
|--------|--------------|----------|
| Sender encrypts fake value | ❌ No | Same value used for cm AND plaintext — cm check would fail |
| Sender encrypts fake recipient | ❌ No | Same recipient used for cm AND plaintext — cm check would fail |
| Sender uses wrong auditor key | ❌ No | fvk_commit must have valid auditor signature |
| Sender uses self-generated FVK | ❌ No | Signature verification fails — not signed by auditor |
| Sender publishes garbage ciphertext | ❌ No | Sequencer checks hash(ct) == ct_hash_arg |
| Attacker modifies ciphertext | ❌ No | MAC verification fails |
| Sender skips auditor attestation | ❌ No | Sequencer rejects tx without signed fvk_commit |
| Sender creates undecryptable blob | ❌ No | Circuit COMPUTES the ciphertext from real data |
| User A decrypts User B's tx | ❌ No | Each user has unique FVK — only auditor has all keys |

The key insight is that **the circuit doesn't accept pre-computed ciphertext** — it derives everything from the same private inputs that create the note commitment. Combined with the sequencer's hash verification and mandatory auditor enforcement, this makes it mathematically impossible to have a valid, accepted transaction with mismatched or fake encrypted data.

---

## Frequently Asked Questions

### Q: Why not use a single shared FVK for all users?

**A:** The FVK is a **symmetric key** — anyone who has it can decrypt. If all users shared the same FVK:

```
PROBLEM WITH SHARED FVK:
════════════════════════════════════════════════════════════════════════

   AUDITOR gives same fvk to everyone:
   
   User A has: fvk_shared
   User B has: fvk_shared  (same key!)
   User C has: fvk_shared  (same key!)
   
   Result:
   • User A can decrypt User B's transactions ❌
   • User B can decrypt User C's transactions ❌
   • Everyone can see everyone's private data ❌
   
   This completely defeats the purpose of privacy!
```

Instead, each user gets a **unique FVK**, and only the auditor stores all of them:

```
SOLUTION WITH UNIQUE FVKs:
════════════════════════════════════════════════════════════════════════

   AUDITOR gives unique fvk to each user:
   
   User A has: fvk_A (unique)
   User B has: fvk_B (unique)
   User C has: fvk_C (unique)
   
   AUDITOR stores: [fvk_A, fvk_B, fvk_C, ...]
   
   Result:
   • User A can only decrypt their own transactions ✅
   • User B can only decrypt their own transactions ✅
   • Only AUDITOR can decrypt ALL transactions ✅
   
   Privacy preserved between users!
```

---

### Q: Why use signatures instead of a hardcoded fvk_commit?

**A:** With unique FVKs, each user has a different `fvk_commit`. We can't hardcode all of them. Instead:

```
SIGNATURE-BASED VERIFICATION:
════════════════════════════════════════════════════════════════════════

   What's hardcoded in nodes:  auditor_pk (public signing key)
   
   What changes per user:      fvk_commit + signature
   
   How it works:
   
   1. Auditor signs each user's fvk_commit with auditor_sk
   2. User includes fvk_commit + signature in transaction
   3. Node verifies: Verify(auditor_pk, signature, fvk_commit)
   
   This proves the fvk_commit was issued by the real auditor,
   without the node needing to know every possible fvk_commit!
```

---

### Q: Why sign the fvk_commit instead of the fvk itself?

**A:** Security! The `fvk` is secret, but the signature needs to be publicly verifiable:

```
WHY SIGN THE HASH (fvk_commit) NOT THE SECRET (fvk):
════════════════════════════════════════════════════════════════════════

   If we signed the fvk directly:
   
   signature = Sign(auditor_sk, fvk)
   
   Problem: To verify this signature, you'd need to see the fvk!
   Anyone verifying would learn the secret key. ❌
   
   ─────────────────────────────────────────────────────────────────────
   
   By signing the hash (fvk_commit):
   
   fvk_commit = H(fvk)           ← one-way, hides fvk
   signature = Sign(auditor_sk, fvk_commit)
   
   Anyone can verify: Verify(auditor_pk, signature, fvk_commit)
   But they only see fvk_commit, not fvk! ✅
   
   The secret fvk remains known only to:
   • The user (to encrypt in the ZK proof)
   • The auditor (to decrypt later)
```

---

### Q: Can a user reuse their FVK for multiple transactions?

**A:** Yes! The user can choose to:

1. **Reuse the same FVK** for all their transactions — simpler, fewer requests to auditor
2. **Request a new FVK** for each transaction — more privacy (harder to link transactions)

Both are valid. The auditor stores all issued FVKs regardless.

---

### Q: What if the auditor service is offline when I need an FVK?

**A:** If you already have a signed FVK from a previous request, you can reuse it. If you need a new one and the auditor service is down, you'd need to wait. This is a trade-off for having regulatory compliance built into the protocol.

---

### Q: Can the auditor be compromised?

**A:** The auditor's security is critical:

| Component | If Compromised | Impact |
|-----------|----------------|--------|
| `auditor_sk` (signing key) | Attacker can issue fake FVKs | Transactions with fake FVKs would be accepted, but auditor couldn't decrypt them |
| FVK database | Attacker can decrypt all transactions | Full privacy breach for affected users |

This is why the auditor should be a trusted, well-secured entity (regulatory body, institutional custodian, etc.).

---

### Q: Is this better than asymmetric encryption (like public-key crypto)?

**A:** Each approach has trade-offs:

| Approach | Pros | Cons |
|----------|------|------|
| **Symmetric (our model)** | Faster, simpler ZK circuits, smaller proofs | Requires auditor service for key distribution |
| **Asymmetric (PKI)** | No key distribution service needed | Slower, more complex ZK circuits, larger proofs |

We chose symmetric encryption with signed key distribution because:
1. ZK proof generation is already expensive — simpler crypto helps performance
2. The auditor service provides additional benefits (key revocation, rate limiting, identity binding)
3. The signature verification at the node level is cheap and standard
