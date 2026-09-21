// covenant-vm executes Rust-generated Woodland covenant fixtures against the
// unmodified, pinned Arkade emulator. It never connects to a wallet or server.
package main

import (
	"bytes"
	"encoding/hex"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"os"

	"github.com/arkade-os/arkd/pkg/ark-lib/extension"
	"github.com/arkade-os/emulator/pkg/arkade"
	"github.com/btcsuite/btcd/chaincfg/chainhash"
	"github.com/btcsuite/btcd/txscript"
	"github.com/btcsuite/btcd/wire"
)

type vector struct {
	Name        string    `json:"name"`
	Script      string    `json:"script"`
	Witness     []string  `json:"witness"`
	Transaction string    `json:"transaction"`
	InputIndex  *int      `json:"input_index"`
	Prevouts    []prevout `json:"prevouts"`
	Valid       *bool     `json:"valid"`
}

type prevout struct {
	Outpoint struct {
		TxID string `json:"txid"`
		Vout uint32 `json:"vout"`
	} `json:"outpoint"`
	Txout struct {
		Value  int64  `json:"value"`
		Script string `json:"script"`
	} `json:"txout"`
	ArkTx      string `json:"ark_tx"`
	VtxoScript string `json:"vtxo_script"`
}

type fixtureFetcher struct {
	outputs map[wire.OutPoint]*wire.TxOut
	arkTxs  map[wire.OutPoint]*wire.MsgTx
	scripts map[wire.OutPoint][]byte
}

func (f *fixtureFetcher) FetchPrevOutput(op wire.OutPoint) *wire.TxOut {
	return f.outputs[op]
}

func (f *fixtureFetcher) FetchPrevOutArkTx(op wire.OutPoint) *wire.MsgTx {
	return f.arkTxs[op]
}

func (f *fixtureFetcher) FetchVtxoPrevOutPkScript(op wire.OutPoint) []byte {
	return f.scripts[op]
}

func decodeTransaction(encoded string) (*wire.MsgTx, error) {
	data, err := hex.DecodeString(encoded)
	if err != nil {
		return nil, err
	}
	reader := bytes.NewReader(data)
	tx := wire.NewMsgTx(2)
	if err := tx.Deserialize(reader); err != nil {
		return nil, err
	}
	if reader.Len() != 0 {
		return nil, errors.New("trailing bytes after transaction")
	}
	return tx, nil
}

type preparedVector struct {
	vector
	tx      *wire.MsgTx
	script  []byte
	witness [][]byte
	fetcher *fixtureFetcher
}

// Fixture decoding is separate from VM rejection: malformed test data must not
// accidentally make an expected-invalid covenant test pass.
func prepare(v vector) (*preparedVector, error) {
	if v.Name == "" || v.Valid == nil || v.InputIndex == nil {
		return nil, errors.New("name, valid, and input_index are required")
	}
	tx, err := decodeTransaction(v.Transaction)
	if err != nil {
		return nil, fmt.Errorf("transaction: %w", err)
	}
	if *v.InputIndex < 0 || *v.InputIndex >= len(tx.TxIn) {
		return nil, errors.New("input_index is outside transaction inputs")
	}
	script, err := hex.DecodeString(v.Script)
	if err != nil {
		return nil, fmt.Errorf("script: %w", err)
	}
	witness := make([][]byte, len(v.Witness))
	for i, item := range v.Witness {
		witness[i], err = hex.DecodeString(item)
		if err != nil {
			return nil, fmt.Errorf("witness %d: %w", i, err)
		}
	}
	fetcher := &fixtureFetcher{
		outputs: make(map[wire.OutPoint]*wire.TxOut),
		arkTxs:  make(map[wire.OutPoint]*wire.MsgTx),
		scripts: make(map[wire.OutPoint][]byte),
	}
	for i, previous := range v.Prevouts {
		if len(previous.Outpoint.TxID) != 64 {
			return nil, fmt.Errorf("prevout %d: txid must contain 32 bytes", i)
		}
		hash, err := chainhash.NewHashFromStr(previous.Outpoint.TxID)
		if err != nil {
			return nil, fmt.Errorf("prevout %d txid: %w", i, err)
		}
		outpoint := wire.OutPoint{Hash: *hash, Index: previous.Outpoint.Vout}
		if _, found := fetcher.outputs[outpoint]; found {
			return nil, fmt.Errorf("duplicate prevout %s", outpoint)
		}
		if previous.Txout.Value < 0 {
			return nil, fmt.Errorf("prevout %d: negative value", i)
		}
		pkScript, err := hex.DecodeString(previous.Txout.Script)
		if err != nil {
			return nil, fmt.Errorf("prevout %d script: %w", i, err)
		}
		arkTx, err := decodeTransaction(previous.ArkTx)
		if err != nil {
			return nil, fmt.Errorf("prevout %d ark_tx: %w", i, err)
		}
		vtxoScript, err := hex.DecodeString(previous.VtxoScript)
		if err != nil {
			return nil, fmt.Errorf("prevout %d vtxo_script: %w", i, err)
		}
		fetcher.outputs[outpoint] = wire.NewTxOut(previous.Txout.Value, pkScript)
		fetcher.arkTxs[outpoint] = arkTx
		fetcher.scripts[outpoint] = vtxoScript
	}
	for i, input := range tx.TxIn {
		if fetcher.FetchPrevOutput(input.PreviousOutPoint) == nil {
			return nil, fmt.Errorf("missing prevout for input %d", i)
		}
	}
	return &preparedVector{v, tx, script, witness, fetcher}, nil
}

func execute(v *preparedVector) error {
	// Mirror ArkadeScript.Execute's production engine and extension setup. These
	// vectors exercise covenant bytecode; Bitcoin signer closures are covered
	// by the Rust signature tests and live regtest transaction tests.
	input := v.tx.TxIn[*v.InputIndex]
	engine, err := arkade.NewEngine(
		v.script, v.tx, *v.InputIndex, txscript.NewSigCache(100),
		txscript.NewTxSigHashes(v.tx, v.fetcher),
		v.fetcher.FetchPrevOutput(input.PreviousOutPoint).Value, v.fetcher,
	)
	if err != nil {
		return fmt.Errorf("create engine: %w", err)
	}
	ext, err := extension.NewExtensionFromTx(v.tx)
	if err != nil {
		if !errors.Is(err, extension.ErrExtensionNotFound) {
			return fmt.Errorf("parse extension: %w", err)
		}
	} else if packet := ext.GetAssetPacket(); packet != nil {
		engine.SetAssetPacket(packet)
	}
	packet, err := arkade.FindEmulatorPacket(v.tx)
	if err != nil {
		return fmt.Errorf("parse emulator packet: %w", err)
	}
	if packet != nil {
		engine.SetEmulatorPacket(packet)
	}
	engine.SetStack(v.witness)
	return engine.Execute()
}

func readVectors(filename string) ([]vector, error) {
	file, err := os.Open(filename)
	if err != nil {
		return nil, err
	}
	defer file.Close()
	decoder := json.NewDecoder(file)
	decoder.DisallowUnknownFields()
	var vectors []vector
	if err := decoder.Decode(&vectors); err != nil {
		return nil, err
	}
	if len(vectors) == 0 {
		return nil, errors.New("no test vectors")
	}
	var extra any
	if err := decoder.Decode(&extra); err != io.EOF {
		return nil, errors.New("trailing JSON data")
	}
	return vectors, nil
}

func run(filenames []string) error {
	if len(filenames) == 0 {
		return errors.New("usage: covenant-vm <vectors.json> [more-vectors.json ...]")
	}
	names := make(map[string]bool)
	verbose := os.Getenv("WOODLAND_VM_VERBOSE") == "1"
	total, rejected, failures := 0, 0, 0
	for _, filename := range filenames {
		vectors, err := readVectors(filename)
		if err != nil {
			return fmt.Errorf("%s: %w", filename, err)
		}
		for _, fixture := range vectors {
			if names[fixture.Name] {
				return fmt.Errorf("duplicate vector name %q", fixture.Name)
			}
			names[fixture.Name] = true
			prepared, err := prepare(fixture)
			if err != nil {
				return fmt.Errorf("%s: invalid fixture: %w", fixture.Name, err)
			}
			total++
			err = execute(prepared)
			if (err == nil) != *fixture.Valid {
				failures++
				fmt.Fprintf(os.Stderr, "FAIL %s: expected valid=%t, VM error=%v\n", fixture.Name, *fixture.Valid, err)
			} else if err != nil {
				rejected++
				if verbose {
					fmt.Printf("PASS %s (rejected: %v)\n", fixture.Name, err)
				}
			} else if verbose {
				fmt.Printf("PASS %s\n", fixture.Name)
			}
		}
	}
	if failures != 0 {
		return fmt.Errorf("%d of %d covenant vectors failed", failures, total)
	}
	fmt.Printf("Stock Arkade VM: %d vectors passed (%d accepted, %d rejected).\n", total, total-rejected, rejected)
	return nil
}

func main() {
	if err := run(os.Args[1:]); err != nil {
		fmt.Fprintln(os.Stderr, err)
		os.Exit(1)
	}
}
