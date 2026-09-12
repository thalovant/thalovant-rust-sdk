"""Generate immutable listing data and cross-SDK golden cases from public packages.
Use Python3.13 with thalovant[listing]==0.6.5, thalovant-languages==0.1.1,
ovos-spec-tools==1.12.0a1 and langcodes==3.5.1. No local SDK overrides.
"""
import argparse
import importlib.metadata as metadata
import itertools
import json
from pathlib import Path
import unicodedata
import langcodes.data_dicts as cldr
import langcodes.language_distance as distance
import thalovant_languages as languages
from thalovant import as_sentence, listing, speakable
from ovos_spec_tools.language import closest_lang

parser = argparse.ArgumentParser()
parser.add_argument('--test-dir', default='testdata')
parser.add_argument('--data-dir', default='data')
parser.add_argument('--fixtures-only', action='store_true')
args = parser.parse_args()
versions = {'thalovant':'0.6.5','thalovant-languages':'0.1.1','ovos-spec-tools':'1.12.0a1','langcodes':'3.5.1'}
for package, expected in versions.items():
    assert metadata.version(package) == expected, f'Install {package}=={expected}'
assert unicodedata.unidata_version == '15.1.0', 'Use Python3.13 for reproducible Unicode data'
assert languages.root() == languages.DATA_ROOT, 'Remove THALOVANT_LANGUAGES_DIR overrides'
keys = {'trailing_words','question_openers','question_words_anywhere','question_patterns','written_forms','slot_examples'}
data = {'sentence_ends':listing.sentence_ends(), 'languages':{
    tag:{key:value for key,value in languages.language(tag).items() if key in keys} for tag in languages.described()}}
source = {'packages':versions,'unicode_version':unicodedata.unidata_version}

def dump(path, value):
    path.parent.mkdir(parents=True,exist_ok=True)
    path.write_text(json.dumps(value,ensure_ascii=False,sort_keys=True,separators=(',',':'))+'\n',encoding='utf-8')

def vectors(path, cases):
    path.parent.mkdir(parents=True,exist_ok=True)
    path.write_text('{"source":'+json.dumps(source)+',"cases":[\n'+',\n'.join(json.dumps(row,ensure_ascii=False,separators=(',',':')) for row in cases)+'\n]}\n',encoding='utf-8')

if not args.fixtures_only:
    root = Path(args.data_dir)
    dump(root/'listing.json',data)
    dump(root/'language-matching.json',{
        'likely':cldr.LIKELY_SUBTAGS,'languages':cldr.LANGUAGE_REPLACEMENTS,
        'scripts':cldr.SCRIPT_REPLACEMENTS,'territories':cldr.TERRITORY_REPLACEMENTS,
        'default_scripts':cldr.DEFAULT_SCRIPTS,'macrolanguages':cldr.NORMALIZED_MACROLANGUAGES,
        'distances':distance.LANGUAGE_DISTANCES,
        'regions':{key:sorted(getattr(distance,key)) for key in ['US','AMERICAS','LATIN_AMERICA','MAGHREB','CNSAR']}})
    dump(root/'unicode-upper.json',{'unicode_version':unicodedata.unidata_version,
        'expansions':{chr(i):chr(i).upper() for i in range(0x110000) if len(chr(i).upper())>1}})
    dump(root/'provenance.json',source)
    for package,target in [('langcodes','LICENSE-langcodes'),('thalovant-languages','LICENSE-languages')]:
        dist = metadata.distribution(package)
        path = next(path for path in dist.files if '/licenses/LICENSE' in str(path))
        Path(target).write_text(dist.locate_file(path).read_text(encoding='utf-8'),encoding='utf-8')

cases = []
for lang,rules in data['languages'].items():
    for text in ['go home','weather in','what time is it','quelle heure est-il','prends rendez-vous avec le docteur','do i need a jacket','como esta el tiempo','',' déjà fini!','𐐨 test','ß test']:
        cases.append({'kind':'sentence','text':text,'lang':lang,'expected':as_sentence(text,lang)})
    for key in rules.get('slot_examples',{}):
        text='open {'+key+'}'
        cases.append({'kind':'speakable','text':text,'lang':lang,'expected':speakable(text,lang=lang)})
phrases=['aqi','air quality','weather in','what is the weather in {location}','what is the air quality like today']
for lang in [*data['languages'],None,'zh']:
    cases.append({'kind':'rank','phrases':phrases,'lang':lang,'expected':listing.rank(phrases,lang)})
vectors(Path(args.test_dir)/'listing-vectors.json',cases)
langs=['en','en-US','en-GB','en-AU','en-IN','fr','fr-CA','fr-FR','fr-CH','es','es-419','es-AR','es-ES','pt','pt-Latn','pt-PT','pt-BR','pt-AO','zh','zh-CN','zh-TW','zh-HK','zh-Hant','zh-Hans','cmn','zh-cmn','yue','ja','de','gsw','sr','sh','sr-Latn','sr-Cyrl','hr','bs','nb','no','nn','he','iw','az','az-Arab','tr','xq','xq-ZZ','und','und-Arab','und-CH','en_US','fr_CA','en-UK','tl','tgl','fil']
sets=[['en-US','en-GB'],['fr-FR','fr-CA'],['pt-BR','pt-PT'],['es-ES','es-419','es-AR'],['zh-CN','zh-TW','zh-HK'],['sr-Cyrl','sr-Latn','hr'],['no','nb','nn'],['tr','az','az-Arab'],['fr','de','en'],['xq','en'],['fr_CA','fr-FR'],['en-US','en'],['pt','pt-BR'],['he'],['tl','fil'],['ar','fa'],['ja','zh'],['en-GB','en-US']]
vectors(Path(args.test_dir)/'language-matching-vectors.json',[{'target':lang,'available':available,'expected':closest_lang(lang,available)} for lang,available in itertools.product(langs,sets)])
