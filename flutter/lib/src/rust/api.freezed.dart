// GENERATED CODE - DO NOT MODIFY BY HAND
// coverage:ignore-file
// ignore_for_file: type=lint
// ignore_for_file: unused_element, deprecated_member_use, deprecated_member_use_from_same_package, use_function_type_syntax_for_parameters, unnecessary_const, avoid_init_to_null, invalid_override_different_default_values_named, prefer_expression_function_bodies, annotate_overrides, invalid_annotation_target, unnecessary_question_mark

part of 'api.dart';

// **************************************************************************
// FreezedGenerator
// **************************************************************************

// dart format off
T _$identity<T>(T value) => value;
/// @nodoc
mixin _$AnalysisEventWire {

 BigInt get generation;
/// Create a copy of AnalysisEventWire
/// with the given fields replaced by the non-null parameter values.
@JsonKey(includeFromJson: false, includeToJson: false)
@pragma('vm:prefer-inline')
$AnalysisEventWireCopyWith<AnalysisEventWire> get copyWith => _$AnalysisEventWireCopyWithImpl<AnalysisEventWire>(this as AnalysisEventWire, _$identity);



@override
bool operator ==(Object other) {
  return identical(this, other) || (other.runtimeType == runtimeType&&other is AnalysisEventWire&&(identical(other.generation, generation) || other.generation == generation));
}


@override
int get hashCode => Object.hash(runtimeType,generation);

@override
String toString() {
  return 'AnalysisEventWire(generation: $generation)';
}


}

/// @nodoc
abstract mixin class $AnalysisEventWireCopyWith<$Res>  {
  factory $AnalysisEventWireCopyWith(AnalysisEventWire value, $Res Function(AnalysisEventWire) _then) = _$AnalysisEventWireCopyWithImpl;
@useResult
$Res call({
 BigInt generation
});




}
/// @nodoc
class _$AnalysisEventWireCopyWithImpl<$Res>
    implements $AnalysisEventWireCopyWith<$Res> {
  _$AnalysisEventWireCopyWithImpl(this._self, this._then);

  final AnalysisEventWire _self;
  final $Res Function(AnalysisEventWire) _then;

/// Create a copy of AnalysisEventWire
/// with the given fields replaced by the non-null parameter values.
@pragma('vm:prefer-inline') @override $Res call({Object? generation = null,}) {
  return _then(_self.copyWith(
generation: null == generation ? _self.generation : generation // ignore: cast_nullable_to_non_nullable
as BigInt,
  ));
}

}


/// Adds pattern-matching-related methods to [AnalysisEventWire].
extension AnalysisEventWirePatterns on AnalysisEventWire {
/// A variant of `map` that fallback to returning `orElse`.
///
/// It is equivalent to doing:
/// ```dart
/// switch (sealedClass) {
///   case final Subclass value:
///     return ...;
///   case _:
///     return orElse();
/// }
/// ```

@optionalTypeArgs TResult maybeMap<TResult extends Object?>({TResult Function( AnalysisEventWire_Segment value)?  segment,TResult Function( AnalysisEventWire_TrackAnalysis value)?  trackAnalysis,TResult Function( AnalysisEventWire_Done value)?  done,TResult Function( AnalysisEventWire_Failed value)?  failed,required TResult orElse(),}){
final _that = this;
switch (_that) {
case AnalysisEventWire_Segment() when segment != null:
return segment(_that);case AnalysisEventWire_TrackAnalysis() when trackAnalysis != null:
return trackAnalysis(_that);case AnalysisEventWire_Done() when done != null:
return done(_that);case AnalysisEventWire_Failed() when failed != null:
return failed(_that);case _:
  return orElse();

}
}
/// A `switch`-like method, using callbacks.
///
/// Callbacks receives the raw object, upcasted.
/// It is equivalent to doing:
/// ```dart
/// switch (sealedClass) {
///   case final Subclass value:
///     return ...;
///   case final Subclass2 value:
///     return ...;
/// }
/// ```

@optionalTypeArgs TResult map<TResult extends Object?>({required TResult Function( AnalysisEventWire_Segment value)  segment,required TResult Function( AnalysisEventWire_TrackAnalysis value)  trackAnalysis,required TResult Function( AnalysisEventWire_Done value)  done,required TResult Function( AnalysisEventWire_Failed value)  failed,}){
final _that = this;
switch (_that) {
case AnalysisEventWire_Segment():
return segment(_that);case AnalysisEventWire_TrackAnalysis():
return trackAnalysis(_that);case AnalysisEventWire_Done():
return done(_that);case AnalysisEventWire_Failed():
return failed(_that);}
}
/// A variant of `map` that fallback to returning `null`.
///
/// It is equivalent to doing:
/// ```dart
/// switch (sealedClass) {
///   case final Subclass value:
///     return ...;
///   case _:
///     return null;
/// }
/// ```

@optionalTypeArgs TResult? mapOrNull<TResult extends Object?>({TResult? Function( AnalysisEventWire_Segment value)?  segment,TResult? Function( AnalysisEventWire_TrackAnalysis value)?  trackAnalysis,TResult? Function( AnalysisEventWire_Done value)?  done,TResult? Function( AnalysisEventWire_Failed value)?  failed,}){
final _that = this;
switch (_that) {
case AnalysisEventWire_Segment() when segment != null:
return segment(_that);case AnalysisEventWire_TrackAnalysis() when trackAnalysis != null:
return trackAnalysis(_that);case AnalysisEventWire_Done() when done != null:
return done(_that);case AnalysisEventWire_Failed() when failed != null:
return failed(_that);case _:
  return null;

}
}
/// A variant of `when` that fallback to an `orElse` callback.
///
/// It is equivalent to doing:
/// ```dart
/// switch (sealedClass) {
///   case Subclass(:final field):
///     return ...;
///   case _:
///     return orElse();
/// }
/// ```

@optionalTypeArgs TResult maybeWhen<TResult extends Object?>({TResult Function( BigInt generation,  int seg,  List<WireColumn> detail,  List<WireColumn> overview)?  segment,TResult Function( BigInt generation,  double bpm,  String keyName,  String keyCamelot,  double offsetSecs,  Float64List beatsSecs,  Float64List downbeatsSecs,  double confidence)?  trackAnalysis,TResult Function( BigInt generation,  List<WireColumn> detail,  List<WireColumn> overview,  int framesPerCol,  int sampleRate,  BigInt durationFrames)?  done,TResult Function( BigInt generation,  String msg)?  failed,required TResult orElse(),}) {final _that = this;
switch (_that) {
case AnalysisEventWire_Segment() when segment != null:
return segment(_that.generation,_that.seg,_that.detail,_that.overview);case AnalysisEventWire_TrackAnalysis() when trackAnalysis != null:
return trackAnalysis(_that.generation,_that.bpm,_that.keyName,_that.keyCamelot,_that.offsetSecs,_that.beatsSecs,_that.downbeatsSecs,_that.confidence);case AnalysisEventWire_Done() when done != null:
return done(_that.generation,_that.detail,_that.overview,_that.framesPerCol,_that.sampleRate,_that.durationFrames);case AnalysisEventWire_Failed() when failed != null:
return failed(_that.generation,_that.msg);case _:
  return orElse();

}
}
/// A `switch`-like method, using callbacks.
///
/// As opposed to `map`, this offers destructuring.
/// It is equivalent to doing:
/// ```dart
/// switch (sealedClass) {
///   case Subclass(:final field):
///     return ...;
///   case Subclass2(:final field2):
///     return ...;
/// }
/// ```

@optionalTypeArgs TResult when<TResult extends Object?>({required TResult Function( BigInt generation,  int seg,  List<WireColumn> detail,  List<WireColumn> overview)  segment,required TResult Function( BigInt generation,  double bpm,  String keyName,  String keyCamelot,  double offsetSecs,  Float64List beatsSecs,  Float64List downbeatsSecs,  double confidence)  trackAnalysis,required TResult Function( BigInt generation,  List<WireColumn> detail,  List<WireColumn> overview,  int framesPerCol,  int sampleRate,  BigInt durationFrames)  done,required TResult Function( BigInt generation,  String msg)  failed,}) {final _that = this;
switch (_that) {
case AnalysisEventWire_Segment():
return segment(_that.generation,_that.seg,_that.detail,_that.overview);case AnalysisEventWire_TrackAnalysis():
return trackAnalysis(_that.generation,_that.bpm,_that.keyName,_that.keyCamelot,_that.offsetSecs,_that.beatsSecs,_that.downbeatsSecs,_that.confidence);case AnalysisEventWire_Done():
return done(_that.generation,_that.detail,_that.overview,_that.framesPerCol,_that.sampleRate,_that.durationFrames);case AnalysisEventWire_Failed():
return failed(_that.generation,_that.msg);}
}
/// A variant of `when` that fallback to returning `null`
///
/// It is equivalent to doing:
/// ```dart
/// switch (sealedClass) {
///   case Subclass(:final field):
///     return ...;
///   case _:
///     return null;
/// }
/// ```

@optionalTypeArgs TResult? whenOrNull<TResult extends Object?>({TResult? Function( BigInt generation,  int seg,  List<WireColumn> detail,  List<WireColumn> overview)?  segment,TResult? Function( BigInt generation,  double bpm,  String keyName,  String keyCamelot,  double offsetSecs,  Float64List beatsSecs,  Float64List downbeatsSecs,  double confidence)?  trackAnalysis,TResult? Function( BigInt generation,  List<WireColumn> detail,  List<WireColumn> overview,  int framesPerCol,  int sampleRate,  BigInt durationFrames)?  done,TResult? Function( BigInt generation,  String msg)?  failed,}) {final _that = this;
switch (_that) {
case AnalysisEventWire_Segment() when segment != null:
return segment(_that.generation,_that.seg,_that.detail,_that.overview);case AnalysisEventWire_TrackAnalysis() when trackAnalysis != null:
return trackAnalysis(_that.generation,_that.bpm,_that.keyName,_that.keyCamelot,_that.offsetSecs,_that.beatsSecs,_that.downbeatsSecs,_that.confidence);case AnalysisEventWire_Done() when done != null:
return done(_that.generation,_that.detail,_that.overview,_that.framesPerCol,_that.sampleRate,_that.durationFrames);case AnalysisEventWire_Failed() when failed != null:
return failed(_that.generation,_that.msg);case _:
  return null;

}
}

}

/// @nodoc


class AnalysisEventWire_Segment extends AnalysisEventWire {
  const AnalysisEventWire_Segment({required this.generation, required this.seg, required final  List<WireColumn> detail, required final  List<WireColumn> overview}): _detail = detail,_overview = overview,super._();
  

@override final  BigInt generation;
 final  int seg;
 final  List<WireColumn> _detail;
 List<WireColumn> get detail {
  if (_detail is EqualUnmodifiableListView) return _detail;
  // ignore: implicit_dynamic_type
  return EqualUnmodifiableListView(_detail);
}

 final  List<WireColumn> _overview;
 List<WireColumn> get overview {
  if (_overview is EqualUnmodifiableListView) return _overview;
  // ignore: implicit_dynamic_type
  return EqualUnmodifiableListView(_overview);
}


/// Create a copy of AnalysisEventWire
/// with the given fields replaced by the non-null parameter values.
@override @JsonKey(includeFromJson: false, includeToJson: false)
@pragma('vm:prefer-inline')
$AnalysisEventWire_SegmentCopyWith<AnalysisEventWire_Segment> get copyWith => _$AnalysisEventWire_SegmentCopyWithImpl<AnalysisEventWire_Segment>(this, _$identity);



@override
bool operator ==(Object other) {
  return identical(this, other) || (other.runtimeType == runtimeType&&other is AnalysisEventWire_Segment&&(identical(other.generation, generation) || other.generation == generation)&&(identical(other.seg, seg) || other.seg == seg)&&const DeepCollectionEquality().equals(other._detail, _detail)&&const DeepCollectionEquality().equals(other._overview, _overview));
}


@override
int get hashCode => Object.hash(runtimeType,generation,seg,const DeepCollectionEquality().hash(_detail),const DeepCollectionEquality().hash(_overview));

@override
String toString() {
  return 'AnalysisEventWire.segment(generation: $generation, seg: $seg, detail: $detail, overview: $overview)';
}


}

/// @nodoc
abstract mixin class $AnalysisEventWire_SegmentCopyWith<$Res> implements $AnalysisEventWireCopyWith<$Res> {
  factory $AnalysisEventWire_SegmentCopyWith(AnalysisEventWire_Segment value, $Res Function(AnalysisEventWire_Segment) _then) = _$AnalysisEventWire_SegmentCopyWithImpl;
@override @useResult
$Res call({
 BigInt generation, int seg, List<WireColumn> detail, List<WireColumn> overview
});




}
/// @nodoc
class _$AnalysisEventWire_SegmentCopyWithImpl<$Res>
    implements $AnalysisEventWire_SegmentCopyWith<$Res> {
  _$AnalysisEventWire_SegmentCopyWithImpl(this._self, this._then);

  final AnalysisEventWire_Segment _self;
  final $Res Function(AnalysisEventWire_Segment) _then;

/// Create a copy of AnalysisEventWire
/// with the given fields replaced by the non-null parameter values.
@override @pragma('vm:prefer-inline') $Res call({Object? generation = null,Object? seg = null,Object? detail = null,Object? overview = null,}) {
  return _then(AnalysisEventWire_Segment(
generation: null == generation ? _self.generation : generation // ignore: cast_nullable_to_non_nullable
as BigInt,seg: null == seg ? _self.seg : seg // ignore: cast_nullable_to_non_nullable
as int,detail: null == detail ? _self._detail : detail // ignore: cast_nullable_to_non_nullable
as List<WireColumn>,overview: null == overview ? _self._overview : overview // ignore: cast_nullable_to_non_nullable
as List<WireColumn>,
  ));
}


}

/// @nodoc


class AnalysisEventWire_TrackAnalysis extends AnalysisEventWire {
  const AnalysisEventWire_TrackAnalysis({required this.generation, required this.bpm, required this.keyName, required this.keyCamelot, required this.offsetSecs, required this.beatsSecs, required this.downbeatsSecs, required this.confidence}): super._();
  

@override final  BigInt generation;
 final  double bpm;
 final  String keyName;
 final  String keyCamelot;
/// 首拍秒偏移（grid 为空时为 0）。
 final  double offsetSecs;
 final  Float64List beatsSecs;
 final  Float64List downbeatsSecs;
 final  double confidence;

/// Create a copy of AnalysisEventWire
/// with the given fields replaced by the non-null parameter values.
@override @JsonKey(includeFromJson: false, includeToJson: false)
@pragma('vm:prefer-inline')
$AnalysisEventWire_TrackAnalysisCopyWith<AnalysisEventWire_TrackAnalysis> get copyWith => _$AnalysisEventWire_TrackAnalysisCopyWithImpl<AnalysisEventWire_TrackAnalysis>(this, _$identity);



@override
bool operator ==(Object other) {
  return identical(this, other) || (other.runtimeType == runtimeType&&other is AnalysisEventWire_TrackAnalysis&&(identical(other.generation, generation) || other.generation == generation)&&(identical(other.bpm, bpm) || other.bpm == bpm)&&(identical(other.keyName, keyName) || other.keyName == keyName)&&(identical(other.keyCamelot, keyCamelot) || other.keyCamelot == keyCamelot)&&(identical(other.offsetSecs, offsetSecs) || other.offsetSecs == offsetSecs)&&const DeepCollectionEquality().equals(other.beatsSecs, beatsSecs)&&const DeepCollectionEquality().equals(other.downbeatsSecs, downbeatsSecs)&&(identical(other.confidence, confidence) || other.confidence == confidence));
}


@override
int get hashCode => Object.hash(runtimeType,generation,bpm,keyName,keyCamelot,offsetSecs,const DeepCollectionEquality().hash(beatsSecs),const DeepCollectionEquality().hash(downbeatsSecs),confidence);

@override
String toString() {
  return 'AnalysisEventWire.trackAnalysis(generation: $generation, bpm: $bpm, keyName: $keyName, keyCamelot: $keyCamelot, offsetSecs: $offsetSecs, beatsSecs: $beatsSecs, downbeatsSecs: $downbeatsSecs, confidence: $confidence)';
}


}

/// @nodoc
abstract mixin class $AnalysisEventWire_TrackAnalysisCopyWith<$Res> implements $AnalysisEventWireCopyWith<$Res> {
  factory $AnalysisEventWire_TrackAnalysisCopyWith(AnalysisEventWire_TrackAnalysis value, $Res Function(AnalysisEventWire_TrackAnalysis) _then) = _$AnalysisEventWire_TrackAnalysisCopyWithImpl;
@override @useResult
$Res call({
 BigInt generation, double bpm, String keyName, String keyCamelot, double offsetSecs, Float64List beatsSecs, Float64List downbeatsSecs, double confidence
});




}
/// @nodoc
class _$AnalysisEventWire_TrackAnalysisCopyWithImpl<$Res>
    implements $AnalysisEventWire_TrackAnalysisCopyWith<$Res> {
  _$AnalysisEventWire_TrackAnalysisCopyWithImpl(this._self, this._then);

  final AnalysisEventWire_TrackAnalysis _self;
  final $Res Function(AnalysisEventWire_TrackAnalysis) _then;

/// Create a copy of AnalysisEventWire
/// with the given fields replaced by the non-null parameter values.
@override @pragma('vm:prefer-inline') $Res call({Object? generation = null,Object? bpm = null,Object? keyName = null,Object? keyCamelot = null,Object? offsetSecs = null,Object? beatsSecs = null,Object? downbeatsSecs = null,Object? confidence = null,}) {
  return _then(AnalysisEventWire_TrackAnalysis(
generation: null == generation ? _self.generation : generation // ignore: cast_nullable_to_non_nullable
as BigInt,bpm: null == bpm ? _self.bpm : bpm // ignore: cast_nullable_to_non_nullable
as double,keyName: null == keyName ? _self.keyName : keyName // ignore: cast_nullable_to_non_nullable
as String,keyCamelot: null == keyCamelot ? _self.keyCamelot : keyCamelot // ignore: cast_nullable_to_non_nullable
as String,offsetSecs: null == offsetSecs ? _self.offsetSecs : offsetSecs // ignore: cast_nullable_to_non_nullable
as double,beatsSecs: null == beatsSecs ? _self.beatsSecs : beatsSecs // ignore: cast_nullable_to_non_nullable
as Float64List,downbeatsSecs: null == downbeatsSecs ? _self.downbeatsSecs : downbeatsSecs // ignore: cast_nullable_to_non_nullable
as Float64List,confidence: null == confidence ? _self.confidence : confidence // ignore: cast_nullable_to_non_nullable
as double,
  ));
}


}

/// @nodoc


class AnalysisEventWire_Done extends AnalysisEventWire {
  const AnalysisEventWire_Done({required this.generation, required final  List<WireColumn> detail, required final  List<WireColumn> overview, required this.framesPerCol, required this.sampleRate, required this.durationFrames}): _detail = detail,_overview = overview,super._();
  

@override final  BigInt generation;
 final  List<WireColumn> _detail;
 List<WireColumn> get detail {
  if (_detail is EqualUnmodifiableListView) return _detail;
  // ignore: implicit_dynamic_type
  return EqualUnmodifiableListView(_detail);
}

 final  List<WireColumn> _overview;
 List<WireColumn> get overview {
  if (_overview is EqualUnmodifiableListView) return _overview;
  // ignore: implicit_dynamic_type
  return EqualUnmodifiableListView(_overview);
}

 final  int framesPerCol;
 final  int sampleRate;
 final  BigInt durationFrames;

/// Create a copy of AnalysisEventWire
/// with the given fields replaced by the non-null parameter values.
@override @JsonKey(includeFromJson: false, includeToJson: false)
@pragma('vm:prefer-inline')
$AnalysisEventWire_DoneCopyWith<AnalysisEventWire_Done> get copyWith => _$AnalysisEventWire_DoneCopyWithImpl<AnalysisEventWire_Done>(this, _$identity);



@override
bool operator ==(Object other) {
  return identical(this, other) || (other.runtimeType == runtimeType&&other is AnalysisEventWire_Done&&(identical(other.generation, generation) || other.generation == generation)&&const DeepCollectionEquality().equals(other._detail, _detail)&&const DeepCollectionEquality().equals(other._overview, _overview)&&(identical(other.framesPerCol, framesPerCol) || other.framesPerCol == framesPerCol)&&(identical(other.sampleRate, sampleRate) || other.sampleRate == sampleRate)&&(identical(other.durationFrames, durationFrames) || other.durationFrames == durationFrames));
}


@override
int get hashCode => Object.hash(runtimeType,generation,const DeepCollectionEquality().hash(_detail),const DeepCollectionEquality().hash(_overview),framesPerCol,sampleRate,durationFrames);

@override
String toString() {
  return 'AnalysisEventWire.done(generation: $generation, detail: $detail, overview: $overview, framesPerCol: $framesPerCol, sampleRate: $sampleRate, durationFrames: $durationFrames)';
}


}

/// @nodoc
abstract mixin class $AnalysisEventWire_DoneCopyWith<$Res> implements $AnalysisEventWireCopyWith<$Res> {
  factory $AnalysisEventWire_DoneCopyWith(AnalysisEventWire_Done value, $Res Function(AnalysisEventWire_Done) _then) = _$AnalysisEventWire_DoneCopyWithImpl;
@override @useResult
$Res call({
 BigInt generation, List<WireColumn> detail, List<WireColumn> overview, int framesPerCol, int sampleRate, BigInt durationFrames
});




}
/// @nodoc
class _$AnalysisEventWire_DoneCopyWithImpl<$Res>
    implements $AnalysisEventWire_DoneCopyWith<$Res> {
  _$AnalysisEventWire_DoneCopyWithImpl(this._self, this._then);

  final AnalysisEventWire_Done _self;
  final $Res Function(AnalysisEventWire_Done) _then;

/// Create a copy of AnalysisEventWire
/// with the given fields replaced by the non-null parameter values.
@override @pragma('vm:prefer-inline') $Res call({Object? generation = null,Object? detail = null,Object? overview = null,Object? framesPerCol = null,Object? sampleRate = null,Object? durationFrames = null,}) {
  return _then(AnalysisEventWire_Done(
generation: null == generation ? _self.generation : generation // ignore: cast_nullable_to_non_nullable
as BigInt,detail: null == detail ? _self._detail : detail // ignore: cast_nullable_to_non_nullable
as List<WireColumn>,overview: null == overview ? _self._overview : overview // ignore: cast_nullable_to_non_nullable
as List<WireColumn>,framesPerCol: null == framesPerCol ? _self.framesPerCol : framesPerCol // ignore: cast_nullable_to_non_nullable
as int,sampleRate: null == sampleRate ? _self.sampleRate : sampleRate // ignore: cast_nullable_to_non_nullable
as int,durationFrames: null == durationFrames ? _self.durationFrames : durationFrames // ignore: cast_nullable_to_non_nullable
as BigInt,
  ));
}


}

/// @nodoc


class AnalysisEventWire_Failed extends AnalysisEventWire {
  const AnalysisEventWire_Failed({required this.generation, required this.msg}): super._();
  

@override final  BigInt generation;
 final  String msg;

/// Create a copy of AnalysisEventWire
/// with the given fields replaced by the non-null parameter values.
@override @JsonKey(includeFromJson: false, includeToJson: false)
@pragma('vm:prefer-inline')
$AnalysisEventWire_FailedCopyWith<AnalysisEventWire_Failed> get copyWith => _$AnalysisEventWire_FailedCopyWithImpl<AnalysisEventWire_Failed>(this, _$identity);



@override
bool operator ==(Object other) {
  return identical(this, other) || (other.runtimeType == runtimeType&&other is AnalysisEventWire_Failed&&(identical(other.generation, generation) || other.generation == generation)&&(identical(other.msg, msg) || other.msg == msg));
}


@override
int get hashCode => Object.hash(runtimeType,generation,msg);

@override
String toString() {
  return 'AnalysisEventWire.failed(generation: $generation, msg: $msg)';
}


}

/// @nodoc
abstract mixin class $AnalysisEventWire_FailedCopyWith<$Res> implements $AnalysisEventWireCopyWith<$Res> {
  factory $AnalysisEventWire_FailedCopyWith(AnalysisEventWire_Failed value, $Res Function(AnalysisEventWire_Failed) _then) = _$AnalysisEventWire_FailedCopyWithImpl;
@override @useResult
$Res call({
 BigInt generation, String msg
});




}
/// @nodoc
class _$AnalysisEventWire_FailedCopyWithImpl<$Res>
    implements $AnalysisEventWire_FailedCopyWith<$Res> {
  _$AnalysisEventWire_FailedCopyWithImpl(this._self, this._then);

  final AnalysisEventWire_Failed _self;
  final $Res Function(AnalysisEventWire_Failed) _then;

/// Create a copy of AnalysisEventWire
/// with the given fields replaced by the non-null parameter values.
@override @pragma('vm:prefer-inline') $Res call({Object? generation = null,Object? msg = null,}) {
  return _then(AnalysisEventWire_Failed(
generation: null == generation ? _self.generation : generation // ignore: cast_nullable_to_non_nullable
as BigInt,msg: null == msg ? _self.msg : msg // ignore: cast_nullable_to_non_nullable
as String,
  ));
}


}

// dart format on
