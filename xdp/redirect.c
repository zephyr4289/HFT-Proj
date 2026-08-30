// BPF XDP Redirect Program for NEXUS-FEED-01 AF_XDP Transport (doc 09 §3)
// Routes UDP dst port 10000 -> XSK Socket A (Feed 0), 10001 -> XSK Socket B (Feed 1).

#include <linux/bpf.h>
#include <linux/if_ether.h>
#include <linux/ip.h>
#include <linux/udp.h>
#include <bpf/bpf_helpers.h>

struct {
    __uint(type, BPF_MAP_TYPE_XSKMAP);
    __uint(max_entries, 64);
    __uint(key_size, sizeof(__u32));
    __uint(value_size, sizeof(int));
} xsk_map SEC(".maps");

SEC("xdp")
int xdp_redirect_filter(struct xdp_md *ctx) {
    void *data_end = (void *)(long)ctx->data_end;
    void *data = (void *)(long)ctx->data;

    struct ethhdr *eth = data;
    if ((void *)(eth + 1) > data_end)
        return XDP_PASS;

    if (eth->h_proto != __builtin_bswap16(ETH_P_IP))
        return XDP_PASS;

    struct iphdr *ip = (void *)(eth + 1);
    if ((void *)(ip + 1) > data_end)
        return XDP_PASS;

    if (ip->protocol != 17) // IPPROTO_UDP
        return XDP_PASS;

    if (ip->ihl < 5)
        return XDP_PASS;

    struct udphdr *udp = (void *)((__u8 *)ip + (ip->ihl * 4));
    if ((void *)(udp + 1) > data_end)
        return XDP_PASS;

    __u16 dest_port = __builtin_bswap16(udp->dest);

    if (dest_port == 10000) {
        __u32 key = 0; // Feed A
        return bpf_redirect_map(&xsk_map, key, XDP_PASS);
    }

    if (dest_port == 10001) {
        __u32 key = 1; // Feed B
        return bpf_redirect_map(&xsk_map, key, XDP_PASS);
    }

    return XDP_PASS;
}

char _license[] SEC("license") = "Dual MIT/GPL";
