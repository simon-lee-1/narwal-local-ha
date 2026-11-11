#!/bin/bash
# Fresh installation only. This script never starts a service or changes DNS.
set -euo pipefail
umask 077
export PATH=/usr/sbin:/usr/bin:/sbin:/bin
fail() { echo "Narwal install: $1" >&2; exit 1; }
[[ $EUID -eq 0 ]] || fail 'run this installer as root'
[[ $# -eq 2 ]] || fail 'usage: install.sh /absolute/private/install.json /absolute/release/binary'
input=$1
binary=$2
base=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)
[[ $input = /* && $binary = /* ]] || fail 'absolute input paths required'
for command in jq openssl socat systemctl useradd runuser getent realpath stat install sed ss; do
    command -v "$command" >/dev/null || fail 'required system tool missing; see deployment guide'
done
[[ -f $input && ! -L $input && $(stat -c %a "$input") = 600 && $(stat -c %h "$input") = 1 ]] ||
    fail 'input must be a regular single-link mode-600 file'
input_parent=$(dirname -- "$input")
[[ $(stat -c %a "$input_parent") = 700 && $(realpath -- "$input_parent") = "$input_parent" ]] ||
    fail 'input parent must be a canonical mode-700 directory'
[[ -f $binary && ! -L $binary && -x $binary ]] || fail 'release binary is missing'
jq -e '
  def ipv4: type == "string" and
    test("\\A(?:0|[1-9][0-9]{0,2})(?:\\.(?:0|[1-9][0-9]{0,2})){3}\\z") and
    (split(".") | all(.[]; (tonumber >= 0 and tonumber <= 255))) and
    (split(".")[0] | tonumber | . > 0 and . < 224);
  (del(.map_display)|keys|sort) == ["bind_ipv4","mqtt","robot_ipv4","settings"] and
  (if has("map_display") then
    (.map_display|type=="object") and
    (.map_display|keys|sort)==["room_labels","show_trail","transform"] and
    (.map_display.transform|.=="identity" or .=="rotate180" or .=="flip_horizontal" or .=="flip_vertical") and
    (.map_display.room_labels|type=="boolean") and
    (.map_display.show_trail|type=="boolean")
   else true end) and
  (.bind_ipv4|ipv4) and (.robot_ipv4|ipv4) and
  (.mqtt|type=="object") and
  (.mqtt|keys|sort)==["client_id","host","password","port","topic_prefix","username"] and
  (.mqtt.host|ipv4) and
  (.mqtt.port|type=="number" and .>=1 and .<=65535 and floor==.) and
  (.mqtt.username|type=="string" and utf8bytelength>0 and utf8bytelength<=256 and (startswith("REPLACE_")|not)) and
  (.mqtt.password|type=="string" and utf8bytelength>0 and utf8bytelength<=8192 and (startswith("REPLACE_")|not)) and
  (.mqtt.client_id|type=="string" and test("\\A[A-Za-z0-9_-]{1,64}\\z")) and
  (.mqtt.topic_prefix|type=="string" and utf8bytelength>0 and utf8bytelength<=256 and
    (test("[+#[:cntrl:]]")|not) and (split("/")|all(.[];length>0))) and
  (.settings|keys|sort) == ["rooms","selected","version"] and
  .settings.version == 1 and (.settings.rooms|length>=1 and length<=32) and
  (.settings.rooms|all(.[]; (keys|sort)==["id","name"] and
    (.id|type=="number" and floor==. and .>=0 and .<=255) and
    (.name|type=="string" and utf8bytelength>=1 and utf8bytelength<=128 and
      (test("[[:cntrl:]]")|not) and test("\\S")))) and
  ([.settings.rooms[].id]|length == (unique|length)) and
  ([.settings.rooms[].name]|length == (unique|length)) and
  (.settings.selected|keys|sort)==["dampness","passes","rooms"] and
  (.settings.selected.passes|.==1 or .==2 or .==3) and
  (.settings.selected.dampness|.=="Normal" or .=="Wet") and
  (.settings.selected.rooms|length>=1 and length<=32 and length==(unique|length)) and
  ([.settings.rooms[].id] as $ids | .settings.selected.rooms | all(.[]; . as $id | $ids|index($id)!=null))
' "$input" >/dev/null 2>&1 || fail 'invalid installation configuration'
bind_ip=$(jq -r .bind_ipv4 "$input")
robot_ip=$(jq -r .robot_ipv4 "$input")
broker_ip=$(jq -r .mqtt.host "$input")
for path in /opt/narwal-local-ha /var/lib/narwal-local-ha /etc/narwal-local-ha \
    /etc/systemd/system/narwal-local-ha.service /etc/systemd/system/narwal-local-ha-tls.service; do
    [[ ! -e $path && ! -L $path ]] || fail 'existing installation found; use the upgrade guide'
done
! getent passwd narwal-local-ha >/dev/null || fail 'service account already exists'
! getent group narwal-local-ha >/dev/null || fail 'service group already exists'
[[ -z $(ss -H -ltn 'sport = :443') ]] || fail 'TCP 443 is already listening'
socat_source=$(realpath -- "$(command -v socat)")
[[ $(stat -c %u "$socat_source") = 0 && -f $socat_source ]] || fail 'socat must be root-owned'
openssl version | jq -R -e 'startswith("OpenSSL 3.")' >/dev/null || fail 'OpenSSL 3 is required'
# All input and destination guards above run before the first installation write.
useradd --system --user-group --home-dir /var/lib/narwal-local-ha --no-create-home \
    --shell /usr/sbin/nologin narwal-local-ha
service_uid=$(id -u narwal-local-ha)
service_gid=$(id -g narwal-local-ha)
install -d -m 755 /opt/narwal-local-ha
install -d -m 700 /etc/narwal-local-ha
install -d -m 700 -o "$service_uid" -g "$service_gid" /var/lib/narwal-local-ha
install -m 555 "$binary" /opt/narwal-local-ha/narwal-local-service
install -m 555 "$socat_source" /opt/narwal-local-ha/socat
install -m 555 "$base/wait-for-socket.sh" /opt/narwal-local-ha/wait-for-socket.sh
install -m 600 -o "$service_uid" -g "$service_gid" "$base/profile.json" /var/lib/narwal-local-ha/profile.json
jq --argjson uid "$service_uid" '{
  socket_path:"/run/narwal-local-ha/robot.sock",frontend_uid:$uid,
  frontend_executable:"/opt/narwal-local-ha/socat",http_address:"127.0.0.1:35394",
  profile_file:"/var/lib/narwal-local-ha/profile.json",intent_file:"/var/lib/narwal-local-ha/intent.json",
  map_file:null,mop_settings_file:"/var/lib/narwal-local-ha/settings.json",
  room_editor_file:"/var/lib/narwal-local-ha/room-editor.json",mqtt:.mqtt,
  map_display:(.map_display // {transform:"identity",room_labels:true,show_trail:false})
}' "$input" > /var/lib/narwal-local-ha/config.json
jq '.settings' "$input" > /var/lib/narwal-local-ha/settings.json
openssl req -x509 -newkey rsa:2048 -sha256 -nodes -days 365 \
    -subj /CN=websocket.narwaltech.com -addext subjectAltName=DNS:websocket.narwaltech.com \
    -keyout /var/lib/narwal-local-ha/key.pem -out /var/lib/narwal-local-ha/certificate.pem \
    >/dev/null 2>&1
chown "$service_uid:$service_gid" /var/lib/narwal-local-ha/{config.json,settings.json,key.pem,certificate.pem}
printf 'NARWAL_TLS_LISTEN=OPENSSL-LISTEN:443,pf=ip4,bind=%s,range=%s/32,reuseaddr,fork,max-children=1,so-rcvtimeo=30,so-sndtimeo=30,verify=0,cert=/var/lib/narwal-local-ha/certificate.pem,key=/var/lib/narwal-local-ha/key.pem,openssl-min-proto-version=TLS1.0,openssl-max-proto-version=TLS1.0,ciphers=AES128-SHA:@SECLEVEL=0\n' \
    "$bind_ip" "$robot_ip" > /etc/narwal-local-ha/frontend.env
sed "s/@MQTT_IPV4@/$broker_ip/" "$base/narwal-local-ha.service" > /etc/systemd/system/narwal-local-ha.service
sed "s/@ROBOT_IPV4@/$robot_ip/" "$base/narwal-local-ha-tls.service" > /etc/systemd/system/narwal-local-ha-tls.service
chmod 644 /etc/systemd/system/narwal-local-ha{,-tls}.service
runuser -u narwal-local-ha -- /opt/narwal-local-ha/narwal-local-service \
    --initialize-intent-only /var/lib/narwal-local-ha/config.json
systemctl daemon-reload
echo 'Installed with services stopped. Follow deploy/README.md to configure local DNS and activate.'
