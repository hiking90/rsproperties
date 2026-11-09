# AOSP property 기능 대조

rsproperties가 Android property system(bionic client, libbase/libcutils
helper, init property service)과 비교해 빠뜨린 기능과 동작 차이를 기록한다.

- 비교 기준: 로컬 AOSP checkout `android-17.0.0_r1`
  (`~/Workspace/aosp`, sparse checkout이므로 `git show HEAD:<path>`로 읽음).
  - bionic: `libc/include/sys/system_properties.h`,
    `libc/system_properties/{system_properties,prop_area,contexts_serialized}.cpp`
  - libbase: `system/libbase/{properties.cpp,parsebool.cpp}`,
    `include/android-base/parseint.h`
  - libcutils: `system/core/libcutils/properties.cpp`
  - init: `system/core/init/property_service.cpp`
- 지원 범위: client는 Linux와 Android, `rsproperties-service`는 Linux 전용,
  Android 지원 API level은 28–36이다. Android에서 service를 쓰는 경로와 macOS는
  비교 대상에서 제외한다.
- 분류
  - (a) Linux emulation이라서 생기는 차이
  - (b) 의도적으로 강화한 동작
  - (c) 의도하지 않은 divergence
  - (d) 기능 누락: AOSP에 있는 API가 없다

## 요약

| # | 영역 | 항목 | 분류 | 상태 |
|---|------|------|------|------|
| C1 | client | property 전체 열거(`__system_property_foreach`) | (d) | 이 branch에서 추가 |
| C2 | client | index로 name·value·serial 읽기(`__system_property_read_callback`) | (d) | 이 branch에서 추가 |
| C3 | client | appcompat override 조회(API 35+) | (c) | 이 branch에서 수정 |
| C4 | client | `WaitForProperty` / `WaitForPropertyCreation` | (d) | 이 branch에서 추가 |
| C5 | client | `GetBoolProperty` / `GetIntProperty` 파싱 규칙 | (d), (c) | 이 branch에서 추가 |
| S1 | service | `persist.*` / `next_boot.*` 영속화 | (d) | 이 branch에서 추가 |
| S2 | service | property type 검사(`CheckType`) | (c) | 이 branch에서 수정 |
| S3 | service | `ctl.*`를 일반 property로 저장 | (c) | 이 branch에서 수정 |
| S4 | service | 실패 응답 코드가 항상 `PROP_ERROR` | (c) | 이 branch에서 수정 |
| S5 | service | `ro.property_service.version` 미설정 | (c) | 이 branch에서 수정 |
| S6 | service | 변경 알림 hook 없음 | (a) | 검토 대상 아님 |

## Client

### C1. property 전체 열거

**AOSP.** `__system_property_foreach(cb, cookie)`는 context area마다
`prop_area::foreach`를 호출한다(`contexts_serialized.cpp:151-157`).
`CheckAccessAndOpen()`이 실패한 area, 즉 SELinux가 open을 거부한 area는
건너뛴다. area 안의 순회 순서는 node마다 `left` subtree, 그 node의 `prop`,
`children` subtree, `right` subtree다(`prop_area.cpp:336-360`). 손상된 offset을
만나면 그 area의 순회만 중단하고, `ForEach`는 그 결과를 무시한다. callback은
`prop_info*`만 받으며, 값은 `__system_property_read_callback`으로 읽는다.
libcutils `property_list`는 이 둘을 묶은 wrapper다.

**rsproperties.** trie를 순회하는 코드가 없다. 그래서
`examples/getprop.rs`는 인자 없이 실행하면 "not yet implemented"를 출력하고
exit 1로 끝난다.

**변경.** `SystemProperties::foreach(|PropertyIndex| ...)`를 추가한다.
- context마다 node read lock을 잡은 상태에서 entry offset을 `Vec`에 모은다.
  lock을 놓은 뒤에 callback을 호출한다. 같은 thread에서 같은 `RwLock`의 read
  lock을 다시 잡으면 std 문서상 panic이나 deadlock이 날 수 있다. 이렇게
  하면 callback 안에서 `read_index_with` 같은 다른 읽기 API를 호출해도
  read lock을 두 번 잡지 않는다. 그 대가로 context마다 `Vec` 할당이 한 번
  생긴다. 이 API는 진단용이고 hot path가 아니므로 이 비용을 감수한다.
- 순회 순서는 bionic과 같다. 재귀 대신 explicit stack을 쓰고, 방문한 node
  수가 data 영역에 들어갈 수 있는 node 수를 넘으면 cycle로 보고
  `Error::FileValidation`을 반환한다.
- area를 열 수 없는 context는 bionic처럼 건너뛴다. init 단계에서 손상으로
  빠진 context slot도 건너뛴다.
- bionic과 다른 점(b): 열린 area 안에서 trie 손상을 만나면 순회를 계속하지
  않고 `Err`를 반환한다. 이 crate는 손상과 부재를 구분한다는 정책을 따른다.
  손상된 context보다 앞서 순회한 context의 index는 이미 callback에 전달된
  상태다.
- `getprop` 예제는 인자가 없으면 toybox `getprop`처럼 이름순으로
  `[name]: [value]`를 출력한다.
- 검증: API 36 emulator의 adb shell(SELinux `shell` domain)에서 이 예제의
  출력과 toybox `getprop` 출력이 531줄 모두 일치했다. Linux에서는
  `example_service` + `setprop`으로 넣은 4개 property(`ro.` long value
  포함)가 모두 출력되었다.

### C2. index로 name·value·serial 읽기

**AOSP.** `__system_property_read_callback(pi, cb)`는 `prop_info*`를 받는다.
callback에는 한 seqlock 구간에서 검증한 `(name, value, serial)`이 함께
전달된다(`system_properties.cpp:261-280`). `ro.` property는 serial을
relaxed load로 한 번 읽고 값을 복사하지 않는다. mutable property는
`ReadMutablePropertyValue`가 반환하는 serial을 넘긴다. 이 serial은 값을
읽은 iteration의 serial이며, dirty bit가 켜져 있을 수 있다(그때 값은 dirty
backup slot에서 읽은 이전 값이다). libbase `WaitForProperty`와 `CachedProperty`가
이 함수를 쓴다.

**rsproperties.** `read_with(name, f)`는 이름을 받아 호출할 때마다 trie
lookup을 다시 한다. callback은 value만 받는다. `PropertyIndex`로 값을 읽는
경로가 없고, value와 serial을 같은 seqlock 구간에서 얻을 수도 없다.

**변경.** `SystemProperties::read_index_with(&PropertyIndex, |name, value, serial| ...)`를
추가한다.
- long 여부는 entry 자신의 이름으로 판정한다. bionic `ReadCallback`이
  `pi->name`으로 판정하는 것과 같다. 기존 `read_with`는 lookup에 쓴 이름으로
  판정하지만, index로 읽을 때는 lookup 이름이 없다.
- serial 의미는 bionic과 같다. mutable property의 serial에는 dirty bit가
  켜져 있을 수 있다. 이 serial을 `wait(idx, Some(serial), ..)`에 넘기면 같은
  update의 clean store를 변경 한 번으로 보고한다.
- 다른 instance의 index는 `Error::InvalidArgument`로 거부한다. `update`와
  같은 규칙이다.

### C3. appcompat override 조회 (API 35+)

**AOSP.** `SystemProperties::Init`은 `BIONIC_APPCOMPAT_OVERRIDE` 환경변수가
있으면(값은 보지 않는다) `use_appcompat_override_`를 켠다
(`system_properties.cpp:80-82`). 이 flag가 켜진 프로세스에서 `Find(name)`은
`ro.appcompat_override.<name>`을 먼저 찾는다. 그 이름의 area가 없거나 entry가
없으면 원래 이름을 찾는다(`:168-197`). override entry는 일반 context area에
있다. 쓰기 경로인 `AreaInit`은 이 flag를 켜지 않는다. 환경변수는 zygote가
`__system_properties_zygote_reload()`(`EnableOverrides`, `:145-152`)를
호출할 때 `putenv`로 설정하고, 자식 프로세스가 그대로 물려받는다. zygote가
어떤 app process에서 이 함수를 호출하는지는 frameworks/base가 checkout에
없어서 확인 필요다.

**version별 차이.** 환경변수 경로는 bionic `3bcd5082d`(2026-03-05, "Update
appcompat override for getprop checks")에서 추가되었다. android-1x tag 중에서는
`android-17.0.0_r1`만 이 commit을 포함한다. 이 commit 이전(API 35–36)의
`EnableOverrides`는 in-memory flag만 켠다. 그래서 zygote가 fork한 프로세스만
flag를 물려받고, exec한 프로세스나 별도 reader는 flag가 켜졌는지 알 수
없다. API 36 emulator(`BE2A.250530.026.D1`)에서 다음을 실측했다.
- libc.so에 `ro.appcompat_override.` 문자열은 있고 `BIONIC_APPCOMPAT_OVERRIDE`
  문자열은 없다.
- root로 `debug.rsp.ac=orig`, `ro.appcompat_override.debug.rsp.ac=ovr`를
  설정하고 `BIONIC_APPCOMPAT_OVERRIDE=1 getprop debug.rsp.ac`를 실행하면
  toybox(bionic)는 `orig`를 출력한다.
- Android 16 QPR(API 36) branch에 이 commit이 backport되었는지는 확인 필요다.

**rsproperties.** bionic의 override 조회 분기에 해당하는 코드가 없다.
override가 켜진 프로세스에서는 같은 property에 대해 libc
`__system_property_get`과 rsproperties가 서로 다른 값을 돌려줄 수 있다.

**변경.** read-only instance(`SystemProperties::new`, 전역 reader)를 만들 때
환경변수를 한 번 읽어 flag로 저장한다. flag가 켜져 있으면 `find`와
`read_with`가 bionic처럼 override 이름을 먼저 조회한다.
- Android에서는 `ro.build.version.sdk`가 37 이상일 때만 환경변수를 따른다.
  API 35–36에서 환경변수를 따르면, 같은 프로세스의 libc가 읽지 않는 override
  값을 이 crate만 읽게 된다(위 실측).
- 한계: API 35–36에서 zygote가 fork한 app process(JNI library로 이 crate를
  쓰는 경우)는 libc가 override를 읽지만, 이 crate는 그 사실을 관찰할 방법이
  없어서 원래 이름을 읽는다.
- override 조회는 어떤 오류든 원래 이름 조회로 넘어간다. bionic도 area를
  열 수 없거나(`pa == nullptr`) entry가 없으면 넘어간다. 오류를 그대로
  반환하면 override context를 열 권한이 없는 프로세스에서 모든 읽기가
  실패한다.
- writer instance(`new_area`, `new_area_locked`)에는 적용하지 않는다.
  bionic `AreaInit`과 같다.
- Linux에서는 API level 조건 없이 환경변수만 본다. 기준은 현재 bionic이며,
  Linux에서는 사용자가 직접 설정하지 않는 한 이 변수가 없다.
- 검증: API 36 emulator에서 수정 전 binary는 환경변수가 있으면 `ovr`를
  읽어 toybox(`orig`)와 달랐다. 이 결과에 따라 API 37 조건을 추가했다.
  API 37 emulator(`rsbinder-a17`, android-37.0 google_apis)는 adb가
  `offline`에서 벗어나지 못해 실측하지 못했다. API 37에서 bionic이
  환경변수를 따르는지, `ro.build.version.sdk`가 37인지는 확인 필요다.

### C4. `WaitForProperty` / `WaitForPropertyCreation`

**AOSP** (`libbase/properties.cpp:246-292`).
- `WaitForPropertyCreation(key, timeout)`: `__system_property_find`가
  `nullptr`이면 global serial이 바뀔 때까지 기다린 뒤 다시 찾는다.
- `WaitForProperty(key, expected, timeout)`: 먼저 생성을 기다린다. 그다음
  `read_callback`으로 값과 serial을 함께 읽고, 값이 `expected`와 같으면
  `true`를 반환한다. 다르면 그 serial을 넘겨 `__system_property_wait`로
  기다린다. 두 함수 모두 시작 시각부터 잰 전체 timeout을 지킨다.

**rsproperties.** serial 단위 `wait`만 있다. 같은 동작을 사용자가 직접
조합해야 한다.

**변경.** `SystemProperties::wait_for_creation(name, timeout)`과
`wait_for_value(name, expected, timeout)`를 추가한다. timeout은
`Option<Duration>`이며 `None`은 무기한이다.
- `wait_for_creation`은 `Ok(Some(PropertyIndex))`를 반환한다. libbase는
  `bool`을 반환하지만, index를 돌려주면 호출자가 `find`를 다시 하지 않아도
  된다.
- libbase는 timeout과 futex 실패를 구분하지 않는다. 이 crate는 futex 실패와
  lookup 실패를 `Err`로 반환하고, timeout만 `Ok(None)` / `Ok(false)`로
  반환한다(b). 공개 API `wait`의 `Option`은 이 둘을 구분하지 못하므로, 내부
  wait 결과를 세 갈래(변경, timeout, 실패)로 나눈다. `wait`의 공개 signature는
  바꾸지 않는다.
- 값 비교는 byte 단위로 한다. 값이 UTF-8이 아니면 `expected`와 같지 않은
  것으로 보고 계속 기다린다.

### C5. bool/int 파싱 규칙

**AOSP.**
- libbase `ParseBool`(`parsebool.cpp:23-31`): `1 y yes on true`는 true,
  `0 n no off false`는 false다. 대소문자를 구분하고 공백을 제거하지 않는다.
  그 밖의 값과 빈 값이면 `GetBoolProperty`가 default를 반환한다. libcutils
  `property_get_bool`도 같은 집합을 받는다.
- libbase `ParseInt`(`parseint.h:106-131`): 앞쪽 C `isspace` 문자를 건너뛴다.
  그 위치가 `0x`/`0X`로 시작하면 16진수, 아니면 10진수로 `strtoll`을
  호출한다. 숫자 뒤에 남은 문자가 있으면 실패한다. `-0x10`처럼 부호 뒤에
  `0x`가 오면 10진수로 읽으므로 실패한다. `ParseUint`(`:36-77`)는 `-`로
  시작하면 거부하고, `+`는 받는다.
- libcutils `property_get_int32/64`(`properties.cpp:53-75`): `strtoimax(.., 0)`를
  쓰므로 8진수(`010` = 8)를 받고, 숫자 뒤에 남은 문자(`12abc` = 12)도
  허용한다. 소스 주석은 8진수 허용을 "unwisely"라고 적는다.

**rsproperties.** `get::<i32>`는 Rust `FromStr`을 쓴다. `" 5"`와 `"0x10"`은
실패하고 `get_or`은 default를 반환한다. `"010"`은 10이므로 libbase와
같고 libcutils와 다르다. bool은 `"true"`/`"false"`만 받는다. 이 차이 중
`GetBoolProperty`가 없다는 점만 문서에 있었다.

**변경.**
- 공개 module `parse`에 `parse_bool(&str) -> Option<bool>`과
  `parse_int::<T>(&str) -> Option<T>`를 추가한다. 둘 다 libbase 규칙을 따른다.
  `T`는 sealed trait `parse::Integer`로 제한하며, libbase가 instantiate하는
  `i8..i64`, `u8..u64`에 `isize`, `usize`를 더한다.
- 전역 함수 `get_bool_or(name, default)`와 `get_int_or::<T>(name, default)`를
  추가한다. `GetBoolProperty`, 범위 인자를 생략한 `GetIntProperty` /
  `GetUintProperty`에 대응한다.
- 기존 `get::<T>`의 `FromStr` 동작은 바꾸지 않는다. 바꾸면 이미 배포된
  호출자의 결과가 달라진다.
- libcutils 규칙(8진수, 뒤에 남은 문자 허용)은 따르지 않는다. libbase가 더
  나중에 나온 API이고, libcutils 소스도 8진수 허용을 결함으로 적는다.

## Service (Linux)

### S1. `persist.*` / `next_boot.*` 영속화 — (d)

init은 set이 성공하면 이름이 `persist.` 또는 `next_boot.`로 시작하는 값을
persistent store에 기록한다(`property_service.cpp:414-423`). 부팅할 때 다시
load하고 `ro.persistent_properties.ready=true`를 설정한다(`:1462`).
rsproperties-service는 이 값을 저장하지 않으므로, 재시작하면 모두 잃는다.

**AOSP 세부 동작** (`persistent_properties.cpp`).
- 형식은 proto2 `PersistentProperties { repeated PersistentPropertyRecord
  { optional string name = 1; optional string value = 2; } properties = 1; }`다.
- 쓰기(`WritePersistentProperty`): 파일을 다시 읽고, 같은 이름의 첫 record를
  고치거나 끝에 추가한다. 값이 같으면 쓰지 않는다. `<file>.tmp`(0600,
  `O_NOFOLLOW`)에 쓰고 fsync, rename, 디렉터리 fsync 순서로 진행한다.
  실패는 log만 남기고 set은 성공으로 응답한다.
- 읽기(`LoadPersistentProperties`): 남은 `.tmp`를 지우고 파일을 읽는다.
  파싱에 실패하거나 `persist.`/`next_boot.`가 아닌 이름이 있으면 파일을
  삭제한다. `next_boot.<name>` record는 `<name>`의 값을 바꾸고 제거된 뒤
  파일에 다시 기록된다. 각 record는 `InitPropertySet`으로 설정되므로 type
  검사를 거친다. 그다음 `ro.persistent_properties.ready=true`를 설정한다.

**변경.** `ServiceOptions::persistent_properties_file(path)`로 켠다. 위 규칙을
그대로 따르며, codec은 새 dependency 없이 crate 안에 직접 구현했다(약 150줄).
init과 다른 점은 다음과 같다.
- (a) Android 8 이전의 legacy layout(`/data/property/persist.*` 파일들)은
  읽지 않는다.
- (a) 시작 후에는 record를 메모리에 두고, 쓸 때마다 파일을 다시 읽지
  않는다. 실행 중에 다른 program이 파일을 고치면 다음 persist set이 그
  내용을 덮어쓴다.
- (b) 파일의 디렉터리가 없으면 시작을 실패시킨다. init에서는 이 디렉터리가
  항상 있다.
- 남은 staged record를 덧붙이는 순서는 init에서는 `unordered_map` 순서라
  정해져 있지 않다. 여기서는 파일에 나온 순서를 따른다.
- 손상된 파일은 owner 결정에 따라 init처럼 삭제한다.

### S2. type 검사 — (c)

`property_contexts`의 type(`int`, `bool`, `enum ...`)은 파싱 단계에서 검증되고
trie에도 직렬화된다(`property_info_serializer.rs:74-87`). 그런데 set
handler(`properties_service.rs:227-239`)는 이름과 길이만 검사한다. init
`CheckPermissions`는 type이 맞지 않으면 `PROP_ERROR_INVALID_VALUE`를
반환한다(`:530-533`). `socket_service.rs:54-63`에 문서화된 access model은
SELinux와 SO_PEERCRED 검사 생략만 다루고, type 검사 생략은 언급하지 않는다.

**AOSP 규칙** (`property_type.cpp` `CheckType`): 빈 값은 항상 통과한다.
`string`은 모두 통과, `bool`은 `true`/`false`/`1`/`0`, `int`/`uint`는 libbase
`ParseInt`/`ParseUint`(64 bit), `double`은 `ParseDouble`(`strtod`, 문자열 전체,
`ERANGE` 거부), `size`는 `[0-9]+[gkm]`, `enum a b`는 나열된 값 중 하나여야
한다. 그 밖의 type은 거부한다. `bool`의 허용 집합은 `ParseBool`보다 좁다.

**변경.** `rsproperties-service/src/property_type.rs`에 같은 규칙을
구현했다. type은 새 API `SystemProperties::property_type(name)`(builder)으로
`property_info`에서 읽는다. init과 다른 점: `double`에서 `strtod`가 받는
hex float(`0x1p3`)와 `nan(...)`을 Rust float parser가 받지 않으므로 거부한다.
subnormal 결과에 `strtod`가 `ERANGE`를 설정하는지는 확인 필요다(여기서는
허용한다).

### S3. `ctl.*` 저장 — 저장 동작은 (c), dispatch 없음은 (a)

init은 `ctl.*`를 area에 저장하지 않는다. 권한을 검사한 뒤 control
message로 처리한다(`:505-512`, `:548-549`). build.prop loader는 `ctl.*`,
`sys.powerctl`, `selinux.restorecon_recursive`를 버린다
(`build_property_parser.rs:424`). 하지만 runtime set 경로는 `ctl.start` 같은
이름을 그대로 area에 기록한다.

init은 control message를 queue에 넣고 main thread의 `HandleControlMessage`가
처리한 뒤 응답한다. 성공하면 `PROP_SUCCESS`, service가 없거나 action이
실패하면 `PROP_ERROR_HANDLE_CONTROL_MESSAGE`다(`init.cpp:556-649`).

**변경.** owner 결정에 따라 handler callback을 추가했다.
`ServiceOptions::control_handler(|msg: &ControlMessage| -> bool)`이며,
`ControlMessage`는 `action`(`ctl.` 뒤), `target`(값), `pid`(SO_PEERCRED)를
담는다.
- socket service가 이름을 검사한 뒤(`INVALID_NAME`) handler를 tokio blocking
  pool에서 호출한다. `true`면 `PROP_SUCCESS`, `false`이거나 panic이면
  `PROP_ERROR_HANDLE_CONTROL_MESSAGE`다. properties service로는 넘기지 않으므로
  area에 저장되지 않는다.
- handler가 없으면 `PROP_ERROR_HANDLE_CONTROL_MESSAGE`로 거부한다.
- init이 하는 권한 검사(`CheckControlPropertyPerms`)는 하지 않는다. 다른
  property와 같은 access model이다.

### S4. 실패 응답 코드 — (c)

service는 실패하면 항상 `PROP_ERROR(-1)`를 보낸다. init은
`PROP_ERROR_READ_ONLY_PROPERTY`, `INVALID_NAME`, `INVALID_VALUE`,
`PERMISSION_DENIED`, `SET_FAILED`, `HANDLE_CONTROL_MESSAGE` 등을 구분한다.
client의 `Error::ServiceError { code }`(`errors.rs:80`)에는 Linux에서 항상
-1만 들어온다.

**변경.** `wire::PROP_ERROR_*`에 bionic의 code 값을 추가하고, init과 같은
조건에서 같은 code를 보낸다.
- 읽을 수 없는 field와 한도를 넘는 길이: `READ_DATA`(init `RecvString`).
- 알 수 없는 command: `INVALID_CMD`.
- 이름 검사 실패, 또는 이름에 NUL이 있거나 UTF-8이 아님: `INVALID_NAME`.
- type 불일치, 값이 너무 길거나 NUL이 있거나 UTF-8이 아님: `INVALID_VALUE`.
- 값이 이미 있는 `ro.`: `READ_ONLY_PROPERTY`.
- 그 밖의 저장 실패: `SET_FAILED`.

검사 순서는 init(`CheckPermissions` → `PropertySet`)과 같다. 이름, `ctl.`,
type, 값 길이, 저장 순이다. 공개 `PROP_ERROR`(-1) 상수는 그대로 두었다.

### S5. `ro.property_service.version` — (c), 이 crate의 client에는 영향 없음

init은 `ro.property_service.version=2`를 설정한다(`:1552`). service는 설정하지
않는다. 이 crate의 client는 값이 없으면 V2를 기본으로 쓰므로 영향이 없다.
영향을 받는 것은 bionic 규칙(값이 없으면 V1)을 따르는 다른 client다.

**변경.** build.prop entry를 적용한 뒤 `ro.property_service.version=2`를
설정한다. build.prop가 이미 값을 정했다면 그 값이 남는다(`ro.`는 write-once).
S1의 persist load 뒤에는 `ro.persistent_properties.ready=true`를 설정한다.
persist 파일을 지정하지 않아도 설정한다. init이 항상 설정하기 때문이다.

### S6. 변경 알림 hook — (a)

init은 property가 바뀌면 `PropertyChanged`로 `on property:` trigger를
실행한다(`:183`). service를 내장한 app이 set에 반응할 callback은 없다.
init.rc trigger는 emulation 범위 밖이다. 따라서 parity 결함이 아니라 기능
확장 후보로 분류한다.

## 누락으로 보지 않는 것

- per-property SELinux / SO_PEERCRED 인가: (a)이며 `socket_service.rs:54-63`에
  문서화된 의도적 단순화다.
- `ContextsSplit` / `ContextsPreSplit` layout: `property_info`가 없는 이전
  Android용 layout이다. 지원 범위(API 28–36) 밖이다.
- deprecated API(`__system_property_find_nth`, `__system_property_read`,
  `__system_property_set_filename`).
- `__system_properties_zygote_reload` 자체: zygote 전용이다. 그 결과로 생기는
  override 조회는 C3에서 다룬다.
