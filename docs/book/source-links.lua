-- Manuscript links stay relative; exports name the exact source commit.
local root=assert(os.getenv('PACKDIR_REPO_ROOT'),'Expected source repository root')
local commit=pandoc.system.with_working_directory(root,function()
  return pandoc.pipe('git',{'rev-parse','HEAD'},''):gsub('%s+$','')
end)
assert(commit:match('^[0-9a-f]+$') and #commit==40,'Expected exact source commit')
local source='https://github.com/querygraph/packdir/'
function Link(link)
  local t=link.target
  if t:match('^%a[%w+.-]*:') or t:sub(1,1)=='#' then return link end
  local path=(t:sub(1,3)=='../') and t:sub(4) or t
  assert(path~='' and not path:find('..',1,true),'Unexpected relative source link')
  link.target=source..'blob/'..commit..'/'..path
  return link
end
