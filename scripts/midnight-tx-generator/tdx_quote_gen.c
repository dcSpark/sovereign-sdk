/*
 * TDX Quote Generator - CLI wrapper for tdx_attest library
 * 
 * Usage: tdx_quote_gen <reportdata.bin> <quote_output.dat>
 * 
 * Compatible with Intel TDX attestation library (libtdx-attest)
 * Uses /dev/tdx-attest device
 */

#include <stdlib.h>
#include <stdio.h>
#include <stdint.h>
#include <string.h>
#include "tdx_attest.h"

#define TDX_REPORTDATA_SIZE 64

int main(int argc, char *argv[])
{
    uint32_t quote_size = 0;
    tdx_report_data_t report_data = {{0}};
    tdx_report_t tdx_report = {{0}};
    tdx_uuid_t selected_att_key_id = {0};
    uint8_t *p_quote_buf = NULL;
    FILE *fptr = NULL;
    size_t read_size;

    if (argc != 3) {
        fprintf(stderr, "Usage: %s <reportdata.bin> <quote_output.dat>\n", argv[0]);
        fprintf(stderr, "\n");
        fprintf(stderr, "Generates a TDX attestation quote using the tdx_attest library.\n");
        fprintf(stderr, "\n");
        fprintf(stderr, "Arguments:\n");
        fprintf(stderr, "  reportdata.bin    - Input file with 64-byte REPORTDATA\n");
        fprintf(stderr, "  quote_output.dat  - Output file for TDX quote\n");
        fprintf(stderr, "\n");
        fprintf(stderr, "Example:\n");
        fprintf(stderr, "  echo -n '00000...' | xxd -r -p > reportdata.bin\n");
        fprintf(stderr, "  %s reportdata.bin quote.dat\n", argv[0]);
        return 1;
    }

    // Read REPORTDATA from file
    fptr = fopen(argv[1], "rb");
    if (!fptr) {
        fprintf(stderr, "Error: Cannot open REPORTDATA file: %s\n", argv[1]);
        return 1;
    }

    read_size = fread(report_data.d, 1, TDX_REPORT_DATA_SIZE, fptr);
    fclose(fptr);

    if (read_size != TDX_REPORT_DATA_SIZE) {
        fprintf(stderr, "Error: REPORTDATA must be exactly %d bytes, got %zu\n",
                TDX_REPORT_DATA_SIZE, read_size);
        fprintf(stderr, "Hint: Create with: echo '<hex>' | xxd -r -p > reportdata.bin\n");
        return 1;
    }

    fprintf(stdout, "REPORTDATA (first 32 bytes): ");
    for (int i = 0; i < 32; i++) {
        fprintf(stdout, "%02x", report_data.d[i]);
    }
    fprintf(stdout, "...\n");

    // Get TD Report
    fprintf(stdout, "Getting TD Report...\n");
    if (TDX_ATTEST_SUCCESS != tdx_att_get_report(&report_data, &tdx_report)) {
        fprintf(stderr, "Error: Failed to get TD report\n");
        fprintf(stderr, "Ensure /dev/tdx-attest is accessible and you have permissions\n");
        return 1;
    }
    fprintf(stdout, "✓ TD Report obtained\n");

    // Get TD Quote
    fprintf(stdout, "Getting TD Quote...\n");
    if (TDX_ATTEST_SUCCESS != tdx_att_get_quote(&report_data, NULL, 0, &selected_att_key_id,
        &p_quote_buf, &quote_size, 0)) {
        fprintf(stderr, "Error: Failed to get TD quote\n");
        fprintf(stderr, "Check TDX Quote Generation Service (QGS) is configured\n");
        return 1;
    }

    fprintf(stdout, "✓ TD Quote obtained: %u bytes\n", quote_size);

    // Write quote to output file
    fptr = fopen(argv[2], "wb");
    if (!fptr) {
        fprintf(stderr, "Error: Cannot create output file: %s\n", argv[2]);
        tdx_att_free_quote(p_quote_buf);
        return 1;
    }

    if (fwrite(p_quote_buf, quote_size, 1, fptr) != 1) {
        fprintf(stderr, "Error: Failed to write quote to file\n");
        fclose(fptr);
        tdx_att_free_quote(p_quote_buf);
        return 1;
    }

    fclose(fptr);
    fprintf(stdout, "✓ Quote written to: %s\n", argv[2]);

    // Display quote header (first 32 bytes)
    fprintf(stdout, "\nQuote header (first 32 bytes): ");
    for (uint32_t i = 0; i < 32 && i < quote_size; i++) {
        fprintf(stdout, "%02x", p_quote_buf[i]);
    }
    fprintf(stdout, "\n");

    // Clean up
    tdx_att_free_quote(p_quote_buf);
    
    fprintf(stdout, "\n✓ Success! TDX quote generation complete.\n");
    return 0;
}

