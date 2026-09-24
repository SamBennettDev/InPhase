#!/bin/bash
# Impair the host -> client direction of the stream, on the CLIENT, with netem.
#
#   tests/streaming/netem.sh <profile> [iface] [host-ip]
#   tests/streaming/netem.sh clear
#
# Only UDP from the host is redirected (ingress -> ifb0 -> netem), so SSH, the
# admin forward and signaling stay clean: the impairment lands on exactly the
# traffic whose resilience is being tested (WT datagrams and the IDR stream).
# Needs sudo.
#
# Profiles model what a home network actually does, not worst cases:
#   clean   no impairment (removes any qdisc)
#   jitter  2 ms +/- 4 ms delay, normal distribution, order preserved
#   loss1   1 % random loss
#   burst   Gilbert-Elliott bursts: ~2 % loss arriving in runs (Wi-Fi A-MPDU)
#   wifi    jitter + light bursty loss together
#   cap40   40 Mbit/s bottleneck with a 30 ms queue - below an 80 Mbps target,
#           so the controller must find the rate without stalling the picture
#
# `rate` is set on every delay profile on purpose: without it netem's jitter
# REORDERS packets, which a real access network does not do, and the test
# would measure reordering instead of jitter.
set -eu
PROFILE="${1:?usage: netem.sh <clean|jitter|loss1|burst|wifi|cap40|clear> [iface] [host-ip]}"
IFACE="${2:-${INPHASE_NETEM_IFACE:-$(ip route get 192.168.1.100 | sed -n 's/.* dev \([^ ]*\).*/\1/p')}}"
HOST="${3:-${INPHASE_HOST_IP:-192.168.1.100}}"

clear_all() {
  sudo tc qdisc del dev "$IFACE" handle ffff: ingress 2>/dev/null || true
  sudo tc qdisc del dev ifb0 root 2>/dev/null || true
}

case "$PROFILE" in
  clean|clear) clear_all; echo "netem: cleared on $IFACE"; exit 0 ;;
  jitter) NETEM="delay 2ms 4ms distribution normal rate 900mbit" ;;
  loss1)  NETEM="loss random 1%" ;;
  burst)  NETEM="loss gemodel 0.6% 30% 100% 0%" ;;
  wifi)   NETEM="delay 2ms 3ms distribution normal loss gemodel 0.3% 30% 100% 0% rate 600mbit" ;;
  cap40)  NETEM="rate 40mbit limit 120" ;;
  *) echo "netem: unknown profile $PROFILE" >&2; exit 2 ;;
esac

clear_all
sudo modprobe ifb numifbs=1
sudo ip link set dev ifb0 up
sudo tc qdisc add dev "$IFACE" handle ffff: ingress
sudo tc filter add dev "$IFACE" parent ffff: protocol ip prio 1 u32 \
  match ip src "$HOST"/32 match ip protocol 17 0xff \
  action mirred egress redirect dev ifb0
# shellcheck disable=SC2086
sudo tc qdisc add dev ifb0 root netem $NETEM
echo "netem: $PROFILE ($NETEM) on UDP from $HOST via $IFACE"
