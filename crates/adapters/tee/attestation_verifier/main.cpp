#include <iostream>
#include <stdarg.h>
#include <vector>
// Uses nlohmann-json instead of PicoJSON
// Needs to be specified BEFORE jwt-cpp/jwt.h
// to be taken into account.
#include <jwt-cpp/traits/nlohmann-json/defaults.h> 
#include <jwt-cpp/jwt.h>
#include <AttestationClient.h>
#include <iostream>
#include <string>
#include <curl/curl.h>
#include <algorithm>
#include <thread>
#include <boost/algorithm/string.hpp>
#include "Logger.h"
#include <iostream>
#include <fstream>
#include <openssl/types.h>
#include <openssl/x509.h>
#include <openssl/pem.h>
#include <openssl/bio.h>
#include <nlohmann/json.hpp>

// default guest attestation url
// The VM is hosted on East US 2
// matching with the default URL.
std::string default_attestation_url = "https://sharedeus2.eus2.attest.azure.net";

// Direct conversion of the C# method to C++
// from the original code snippet
// https://github.com/Azure-Samples/microsoft-azure-attestation/blob/aa9fbb2d8869c020c633f3895f43525aaf60bf59/maa.jwt.verifier.dotnet/Utilities.cs#L38C46-L38C83
std::map<std::string, X509*> load_self_signed_certs(const nlohmann::json& jwks)
{
    std::map<std::string, X509*> out;

    for (auto& key : jwks["keys"])
    {
        std::string kid = key.value("kid", "");
        for (auto& x5c : key["x5c"])
        {
            std::string der_b64 = x5c.get<std::string>();
            std::string der = jwt::base::decode<jwt::alphabet::base64>(der_b64);

            const unsigned char* p = reinterpret_cast<const unsigned char*>(der.data());
            X509* cert = d2i_X509(nullptr, &p, der.size());
            if (!cert) continue;

            // self-signed check: subject == issuer DN
            if (X509_NAME_cmp(X509_get_subject_name(cert), X509_get_issuer_name(cert)) == 0)
                out.emplace(kid, cert);
            else
                X509_free(cert);
        }
    }
    if (out.empty()) throw std::runtime_error("no self-signed x5c certs");
    return out;
}

// Computes the SHA256 hash of a file and returns it as a 32-byte array.
// Used to hash all executable files used in the ceremony & attestation process.
std::array<unsigned char, SHA256_DIGEST_LENGTH> compute_file_sha256(const std::string& path) {
    std::array<unsigned char, SHA256_DIGEST_LENGTH> hash{};
    std::ifstream file(path, std::ios::binary);
    if (!file) {
        fprintf(stderr, "Error: could not open file %s\n", path.c_str());
        return hash;
    }

    EVP_MD_CTX* ctx = EVP_MD_CTX_new();
    if (!ctx) {
        fprintf(stderr, "Error: EVP_MD_CTX_new failed\n");
        return hash;
    }

    const EVP_MD* md = EVP_sha256();
    if (EVP_DigestInit_ex(ctx, md, nullptr) != 1) {
        fprintf(stderr, "Error: EVP_DigestInit_ex failed\n");
        EVP_MD_CTX_free(ctx);
        return hash;
    }

    std::array<char, 4096> buffer;
    while (file.read(buffer.data(), buffer.size()) || file.gcount()) {
        if (EVP_DigestUpdate(ctx, buffer.data(), file.gcount()) != 1) {
            fprintf(stderr, "Error: EVP_DigestUpdate failed\n");
            EVP_MD_CTX_free(ctx);
            return hash;
        }
    }

    unsigned int len = 0;
    if (EVP_DigestFinal_ex(ctx, hash.data(), &len) != 1 || len != hash.size()) {
        fprintf(stderr, "Error: EVP_DigestFinal_ex failed\n");
        EVP_MD_CTX_free(ctx);
        return hash;
    }

    EVP_MD_CTX_free(ctx);
    return hash;
}

size_t write_cb(char* ptr, size_t size, size_t nmemb, void* userdata)
{
    auto* out = static_cast<std::string*>(userdata);
    out->append(ptr, size * nmemb);
    return size * nmemb;
}

static std::string jwk_x5c_to_pem(const nlohmann::json& jwk)
{
    if (!jwk.contains("x5c") || jwk["x5c"].empty())
        throw std::runtime_error("JWK has no x5c certificate");

    std::string der_b64 = jwk["x5c"][0].get<std::string>();
    std::string der = jwt::base::decode<jwt::alphabet::base64>(der_b64);

    const unsigned char* p = reinterpret_cast<const unsigned char*>(der.data());
    X509* cert = d2i_X509(nullptr, &p, der.size());
    if (!cert) throw std::runtime_error("X509 decode failed");

    BIO* bio = BIO_new(BIO_s_mem());
    PEM_write_bio_X509(bio, cert);
    BUF_MEM* mem;
    BIO_get_mem_ptr(bio, &mem);
    std::string pem(mem->data, mem->length);

    BIO_free(bio);
    X509_free(cert);
    return pem;
}

std::string trimSlash(std::string s)
{
    if (!s.empty() && s.back() == '/') s.pop_back();
    return s;
};

std::string http_get(const std::string& url)
{
    CURL* curl = curl_easy_init();
    if (!curl) throw std::runtime_error("curl_easy_init failed");

    std::string body;
    curl_easy_setopt(curl, CURLOPT_URL, url.c_str());
    curl_easy_setopt(curl, CURLOPT_FOLLOWLOCATION, 0L); // No redirects
    curl_easy_setopt(curl, CURLOPT_SSL_VERIFYPEER, 1L);
    curl_easy_setopt(curl, CURLOPT_WRITEFUNCTION, write_cb);
    curl_easy_setopt(curl, CURLOPT_WRITEDATA, &body);

    auto rc = curl_easy_perform(curl);
    curl_easy_cleanup(curl);
    if (rc != CURLE_OK)
        throw std::runtime_error("HTTP GET failed: " + std::string(curl_easy_strerror(rc)));
    return body;
}

// Adapted directly from this example:
// https://github.com/Azure-Samples/microsoft-azure-attestation/blob/aa9fbb2d8869c020c633f3895f43525aaf60bf59/maa.jwt.verifier.dotnet/Program.cs#L94
bool ValidateToken(const jwt::decoded_jwt<jwt::traits::nlohmann_json>& decoded, std::string& expectedIssuer, bool validateLifetime)
{
    jwt::traits::nlohmann_json::object_type payload = decoded.get_payload_json();

    // The issuer in the token doesn't end up with /
    // so we need to trim it, in order to pass the validation, in case an issuer was specified with a /.
    expectedIssuer = trimSlash(expectedIssuer);

    // URL where you can pull the signing certificates
    auto header = decoded.get_header_json();
    std::string jku = header["jku"].get<std::string>();
    const std::string expectedJku = expectedIssuer + "/certs";
    if (trimSlash(jku) != expectedJku) {
        fprintf(stderr, "Rejected: jku not pinned. got=%s expected=%s\n", jku.c_str(), expectedJku.c_str());
        return false;
    }
    std::string kid = header["kid"].get<std::string>();
    fprintf(stderr, "Cert URL header: %s\n", jku.c_str());
    fprintf(stderr, "Key ID header: %s\n", kid.c_str());

    // Calling JKU / Azure certs endpoint to key the keys
    nlohmann::json jwks_json = nlohmann::json::parse(http_get(jku));
    
    auto it = std::find_if(jwks_json["keys"].begin(), jwks_json["keys"].end(),
                        [&](const auto& k){ return k["kid"] == kid; });
    if (it == jwks_json["keys"].end()) {
        fprintf(stderr, "kid %s not present under %s\n", kid.c_str(), jku.c_str());
        return false;
    }
    fprintf(stderr, "Using platform leaf kid=%s for JWT verification\n", kid.c_str());
    std::string pem = jwk_x5c_to_pem(*it);

    if (pem.empty()) {
        fprintf(stderr, "Error: PEM conversion failed for JWK with key ID %s\n", kid.c_str());
        return false;
    }

    // As we only verify the token, we can specify the public key and leave the rest of the parameters empty.
    // jwt-cpp does the heavy lifting here regarding the verification itself
    // If validateLifetime is true, we will use the default clock, otherwise we will give a 100 years skew.
    // This allows us to skip the lifetime verification.

    // Note: Audience verification is skipped here, as the JWT does not have the field `aud`
    // Algorithm::rs256 is used as the JWT is signed using RS256
    auto verifier = jwt::verify().allow_algorithm(jwt::algorithm::rs256(pem, "", "", "")).with_issuer(expectedIssuer);

    if (!validateLifetime) {
        // disable all time checks by giving 100 years skew
        verifier = verifier.expires_at_leeway(3153600000);
        fprintf(stderr, "Warning: Lifetime verification has been disabled\n");
    }

    try {
        fprintf(stderr, "Verifying the JWT...\n");
        verifier.verify(decoded);
        return true;
    } catch (const std::exception& e) {
        fprintf(stderr, "Error: Exception occured when verifying the JWT. Details - %s\n", e.what());
        return false;
    }
}

// Encode the hash blob to base64
// I cannot use the jwt-cpp library here, as it does expect a string as input,
// and the hash blob is a vector of bytes...
// OpenSSL encoding methods will be used here.
// Leverage OpenSSL's BIO functions for this purpose.
std::string base64_encode(const unsigned char* input, size_t length) {
    BIO* bio = BIO_new(BIO_s_mem());
    BIO* b64 = BIO_new(BIO_f_base64());
    BIO_set_flags(b64, BIO_FLAGS_BASE64_NO_NL);
    bio = BIO_push(b64, bio);

    BIO_write(bio, input, length);
    BIO_flush(bio);

    BUF_MEM* buffer_ptr;
    BIO_get_mem_ptr(bio, &buffer_ptr);
    std::string result(buffer_ptr->data, buffer_ptr->length);

    BIO_free_all(bio);
    return result;
}

// Decode the base64 encoded string to a vector of bytes
// Unfortunately necessary as jwt-cpp does not support decoding base64 to a vector of bytes directly.
// Leverage OpenSSL's BIO functions for this purpose.
std::vector<std::uint8_t> base64_decode(const std::string& input) {
    BIO* bio = BIO_new_mem_buf(input.data(), static_cast<int>(input.size()));
    BIO* b64 = BIO_new(BIO_f_base64());
    bio = BIO_push(b64, bio);
    BIO_set_flags(bio, BIO_FLAGS_BASE64_NO_NL);  // No newlines

    std::vector<std::uint8_t> output(input.size() * 3 / 4); // Base64 expands 3:4
    int decoded_size = BIO_read(bio, output.data(), static_cast<int>(output.size()));
    output.resize(decoded_size > 0 ? decoded_size : 0);

    BIO_free_all(bio);
    return output;
}

std::string to_hex(const std::uint8_t* p, std::size_t len)
{
    std::ostringstream oss;
    oss << std::hex << std::setfill('0');
    for (std::size_t i = 0; i < len; ++i)
        oss << std::setw(2) << static_cast<int>(p[i]);
    return oss.str();
}

void check_policy(jwt::traits::nlohmann_json::object_type& raw, nlohmann::json& policy)
{
    // The Midnight L2 state root and batch hash are not verified here,
    // they will be verified in the Rust part directly.
    // This function only verifies the attestation fields.

    nlohmann::json payload = raw;
    using pair = std::pair<const char*, const char*>;
    // Map of fields we want to check.
    // Made this way in order to be personalized easily.
    static const std::vector<pair> map = {
        {"attestation-type",            "/x-ms-isolation-tee/x-ms-attestation-type"},
        {"compliance-status",           "/x-ms-isolation-tee/x-ms-compliance-status"},
        {"secureboot",                  "/secureboot"},
        {"vm_id",                       "/x-ms-azurevm-vmid"},
        {"kerneldebug-enabled",         "/x-ms-azurevm-kerneldebug-enabled"},
        {"imageId",                     "/x-ms-isolation-tee/x-ms-sevsnpvm-imageId"},
        {"microcode-svn",               "/x-ms-isolation-tee/x-ms-sevsnpvm-microcode-svn"},
        {"snpfw-svn",                   "/x-ms-isolation-tee/x-ms-sevsnpvm-snpfw-svn"},
        {"launch_measurement",          "/x-ms-isolation-tee/x-ms-sevsnpvm-launchmeasurement"},
    };

    auto check_field = [&](const char* key, const std::string& expected, const std::string& actual) {
        if (expected == actual) {
            fprintf(stderr, "Policy check passed for %s: %s\n", key, actual.c_str());
        } else {
            throw std::runtime_error("policy mismatch @ " + std::string(key) +
                                    "\n  expected: " + expected +
                                    "\n  got     : " + actual);
        }
    };

    for (auto [key, path] : map) {
        nlohmann::json::json_pointer ptr{std::string(path)};
        if (!payload.contains(ptr))
            throw std::runtime_error("attestation missing " + std::string(path));
        if (payload[ptr] == policy[key])
            fprintf(stderr, "Policy check passed for %s: %s\n", key, payload[ptr].dump().c_str());
        else
            throw std::runtime_error("policy mismatch @ " + std::string(key) +
                                    "\n  expected: " + policy[key].dump() +
                                    "\n  got     : " + payload[ptr].dump());
    }
    fprintf(stderr, "Attestation compliant with the policy!\n");
}

void usage(char* programName) {
    fprintf(stderr, "Usage: %s -o <output_file> -n <nonce> -i <payload_input> | -v <input_file> -p <policy_file>\n", programName);
}

int main(int argc, char* argv[]) {
    std::string attestation_url;
    std::string nonce;
    std::string output_file = "";
    std::string input_file;
    std::string hash_file;
    std::string policy_file;
    std::string midnight_payload;
    bool validate_lifetime = true;

    int opt;
    while ((opt = getopt(argc, argv, ":o:v:a:p:i:n:")) != -1) {
        switch (opt) {
        case 'o':
            output_file.assign(optarg);
            break;
        case 'v':
            input_file.assign(optarg);
            break;
        case 'a':
            attestation_url.assign(optarg);
            break;
        case 'p':
            policy_file.assign(optarg);
            break;
        case 'i':
            midnight_payload.assign(optarg);
            break;
        case 'n':
            nonce.assign(optarg);
            break;
        case ':':
            fprintf(stderr, "Option needs a value\n");
            return (1);
        default:
            usage(argv[0]);
            return (1);
        }
    }

    try {
        if (attestation_url.empty()) {
            // use the default attestation url
            attestation_url.assign(default_attestation_url);
        }

        if ((!output_file.empty() && !input_file.empty()) || (!midnight_payload.empty() && !input_file.empty())) {
            fprintf(stderr, "Error: You cannot get and verify an attestation at the same time\n");
            return (1);
        }

        if (policy_file.empty()) {
            policy_file = "policy.json";
        }


        if (nonce.empty()) {
            fprintf(stderr, "Error: You must specify a nonce to get an attestation.\n");
            return (1);
        }

        if (midnight_payload.empty() && input_file.empty()) {
            fprintf(stderr, "Error: You must specify a payload to get an attestation.\n");
            return (1);
        }

        if (input_file.empty() && !midnight_payload.empty()) {
            AttestationClient* attestation_client = nullptr;
            Logger* log_handle = new Logger();

            // Initialize attestation client
            if (!Initialize(log_handle, &attestation_client)) {
                fprintf(stderr, "Failed to create attestation client object\n");
                Uninitialize();
                return (1);
            }

            // Generate the attestation
            // This part is critical as we bind the current attestation with our data.
            // The JWT generated will contains our data and will be signed using Microsoft's key,
            // cryptographically binding the attestation to our data.
            attest::ClientParameters params = {};
            params.attestation_endpoint_url = (unsigned char*)attestation_url.c_str();

            std::string payload = "{\"nonce\":\"" + nonce + "\",\"midnight_payload\":\"" + midnight_payload + "\"}";
            params.client_payload = (unsigned char*) payload.c_str();
            params.version = CLIENT_PARAMS_VERSION; // Version 1
            unsigned char* jwt = nullptr;
            attest::AttestationResult result;
            
            bool is_cvm = false;
            bool attestation_success = true;
            std::string jwt_str;
            // Generate the attestation using MAA.
            // Internally, the HCL report is generated, parsed, 
            // and served under the MAA attestation system.
            if ((result = attestation_client->Attest(params, &jwt)).code_ 
                    != attest::AttestationResult::ErrorCode::SUCCESS) {
                attestation_success = false;
            }
        
            if (attestation_success) {
                jwt_str = reinterpret_cast<char*>(jwt);
                fprintf(stderr, "Attestation generated");
                attestation_client->Free(jwt);
                
                jwt::decoded_jwt<jwt::traits::nlohmann_json> decoded = jwt::decode(jwt_str);
                jwt::traits::nlohmann_json::object_type payload = decoded.get_payload_json();

                try {
                    fprintf(stderr, "Decoded JWT payload:\n");
                    for (const auto& [key, value] : payload) {
                        fprintf(stderr, "  %s: %s\n", key.c_str(), value.dump().c_str());
                    }
                    fprintf(stderr, "\n");
                    fprintf(stderr, "Now verifying if the attestation is from a compliant AMD-SEV-SNP CVM.\n");
                    // Initial check to see if we truly are in a AMD-SEV-SNP
                    // environment. This check is mandatory as MAA works with multiple
                    // attestation system, such as SGX, TDX.
                    const nlohmann::json& tee = payload["x-ms-isolation-tee"];
                    std::string attestation_type  = tee["x-ms-attestation-type"];
                    std::string compliance_status = tee["x-ms-compliance-status"];

                    if (boost::iequals(attestation_type,  "sevsnpvm") &&
                        boost::iequals(compliance_status, "azure-compliant-cvm"))
                    {
                        is_cvm = true;
                        fprintf(stderr, "The running VM is a compliant AMD-SEV-SNP CVM.\n");
                    }
                }
                catch (...) { }
            }

            if (is_cvm) {
                // if no output file is specified, the attestation will only be printed on stdout.
                if (!output_file.empty()) {
                    fprintf(stderr, "Exporting attestation to %s\n", output_file.c_str());
                    std::ofstream attestation_file(output_file);
                    attestation_file << jwt_str;
                    attestation_file.close();
                }
                printf("%s", jwt_str.c_str());
            }
            else {
                fprintf(stderr, "Error: The running VM does not seems to be a compliant AMD-SEV-SNP CVM. The attestation cannot be safely performed\n");
                return (1);
            }

            Uninitialize();
        }
        else if (!input_file.empty()) {
            std::string jwt_str = "";
            if (input_file.substr(input_file.length() - 4) == ".txt") {
                std::ifstream jwt_file(input_file, std::ios::binary);
                if (!jwt_file) {
                    fprintf(stderr, "Error: Could not open JWT file %s\n", input_file.c_str());
                    return (1);
                }
                std::stringstream buffer;
                buffer << jwt_file.rdbuf();
                jwt_str = buffer.str();
                fprintf(stderr, "JWT token read from %s\n", input_file.c_str());
            }
            else {
                jwt_str = input_file;
            }
            fprintf(stderr, "Verifying JWT token: %s\n", jwt_str.c_str());

            jwt::decoded_jwt<jwt::traits::nlohmann_json> decoded = jwt::decode(jwt_str);

            // Validate the token
            // The validateTimelife setting is by default false as this is not required in our POC
            // but it can be activated if you want to check the token lifetime with -t.
            bool is_valid = ValidateToken(decoded, attestation_url, validate_lifetime);
            if (is_valid) {
                fprintf(stderr, "JWT token is valid.\n");
            } else {
                fprintf(stderr, "Error: JWT token is invalid.\n");
                exit(1);
            }

            // The OID extension (1.3.6.1.4.1.311.105.1000.1) is not used here, as the current report (06/12/2025) embedded everything already in 
            // x-ms-isolation-tee.
            // This seems to math with the following documentation from Microsoft: https://github.com/MicrosoftDocs/azure-docs/blob/main/articles/confidential-computing/guest-attestation-confidential-vms.md#json-web-token
            // An initial attempt was made to extract the data from it, but this extension is missing from all keys. The available extensions are only for SGX.
            // The rest of this CLI will only parse the x-ms-isolation-tee field.

            jwt::traits::nlohmann_json::object_type payload = decoded.get_payload_json();
            std::string policy_str = "";
            if (policy_file.substr(policy_file.length() - 5) == ".json") {
                std::ifstream policy(policy_file);
                if (!policy) {
                    fprintf(stderr, "Error: Could not open policy file %s\n", policy_file.c_str());
                    fprintf(stderr, "The attestation cannot be validated fully without a policy file.");
                    exit(1);
                }
                std::stringstream buffer;
                buffer << policy.rdbuf();
                policy_str = buffer.str();
                fprintf(stderr, "Policy file read from %s\n", policy_file.c_str());
                policy.close();
            }
            else {
                policy_str = policy_file;
            }
            // Validate the policy. 
            // Not a lot of field are verified at the moment, but this can always be changed.
            nlohmann::json policy_json = nlohmann::json::parse(policy_str);
            check_policy(payload, policy_json);
            fprintf(stderr, "Policy check passed.\n");
            printf("Attestation verified successfully.\n");
        }
    }
    catch (std::exception& e) {
        fprintf(stderr, "Error: Exception occured. Details: %s\n", e.what());
        return (1);
    }
    return (0);
}
