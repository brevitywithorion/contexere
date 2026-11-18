# `contexere doctor`

Memeriksa bahwa pemasangannya sehat, dan memperbaiki yang bisa ia perbaiki.

```sh
contexere doctor
contexere doctor --fix
contexere doctor --mcp
```

Ia mencakup versi dan keterjangkauan binary-nya, direktori konfigurasi dan basis
data, pemasangan hook per host, pendaftaran server MCP, dan pemuatan sinyal.

## Flag

| flag | efeknya |
|---|---|
| `--fix` | Perbaiki masalah konfigurasi dan integrasi secara otomatis |
| `--detail` | Cetak semua baris integrasi, bukan hanya yang perlu perhatian |
| `--json` | Bisa dibaca mesin |
| `--mcp` | Periksa apakah setiap server MCP yang dikonfigurasi menjawab |
| `--help`, `-h` | Bantuan |

## Membaca keluarannya

**Tingkat host.** `doctor` mencetak tingkat untuk setiap host yang terpasang, dan
tingkat itu adalah langit-langit jujur untuk apa yang bisa dilakukan Contexere di
sana. Host yang mengutamakan handoff atau hanya MCP tidak bisa menulis ulang
keluaran tool shell bawaannya, jadi sebanyak apa pun pekerjaan pipeline tidak
akan menggerakkan angka penyulingannya. Lihat
[Agent yang didukung](../agents.md).

**`[N UNRELEASED]`.** Build yang dikompilasi dari pohon sumber yang
`CHANGELOG.md`-nya masih punya entri di bawah `## [Unreleased]` akan
mengatakannya, dan menyuruh Anda memotong sebuah tag. Pada build rilis tidak ada
baris seperti itu. Ini ada supaya binary yang ditandai tanpa memindahkan entri
changelog-nya menuduh dirinya sendiri, bukan terkirim diam-diam.

**Hitungan retensi terkini.** Berapa banyak yang ada di setiap tingkat ingatan
saat ini.

## `--mcp`

Jalannya yang biasa tidak menjalankan program lain. `contexere doctor --mcp`
melakukannya, sekali per server dari daftar yang sama dengan
`contexere carry list`. Untuk masing-masing ia mengirim handshake
MCP `initialize` dan `tools/list`, lalu mencetak satu baris: terjangkau dengan
jumlah perkakas, gagal dengan alasannya, atau dilewati karena sebuah kunci yang
ia butuhkan belum disetel. Server yang tidak menjawab dihentikan setelah
beberapa detik. Tidak ada yang ditulis, dan nilai env serta header tidak
dicetak.

## Yang tidak ia periksa

Bahwa host-nya benar-benar menerapkan hasil tulis ulangnya. `doctor` memverifikasi
konfigurasinya ada di tempat host membacanya, dan itu tidak sama dengan host
menghormatinya. Buktinya adalah sebuah baris penyulingan di basis data di bawah
`agent_id` host Anda, atau transkrip sesi host itu sendiri.

Di Claude Code, muatan hook yang ditolak host dicatat sebagai lampiran yang tidak
pernah sampai ke model, jadi agent bisa yakin semuanya baik-baik saja sementara
terminal Anda penuh peringatan:

```sh
grep -c hook_error_during_execution ~/.claude/projects/<proyek>/<sesi>.jsonl
```
